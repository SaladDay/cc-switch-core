use cc_switch_core::{
    builtin_app_adapter, builtin_app_adapters, codex::ProviderTableSyntax, AppType,
    CodexImportClassification, CodexImportPolicy, CodexImportPresence, CodexImportValidation,
    LiveDocumentSet, LiveDocumentSetError, LogicalTarget, NativeImportCandidate, NativeImportError,
    NativeImportPolicy, NativeImportStep, NativeProviderMode, ObservedDocument,
    MAX_OPERATION_CONTENT_BYTES,
};
use serde_json::json;

fn documents(auth: Option<&str>, config: Option<&str>) -> LiveDocumentSet {
    LiveDocumentSet::try_new(
        AppType::Codex,
        builtin_app_adapter(&AppType::Codex)
            .targets()
            .iter()
            .copied()
            .map(|target| {
                let contents = match target {
                    LogicalTarget::CodexAuth => auth,
                    LogicalTarget::CodexConfig => config,
                    _ => return ObservedDocument::unobserved(target),
                };
                contents.map_or_else(
                    || ObservedDocument::missing(target),
                    |text| ObservedDocument::present(target, text.as_bytes()),
                )
            }),
    )
    .unwrap()
}

fn snapshot_policy() -> NativeImportPolicy {
    NativeImportPolicy::Codex(CodexImportPolicy {
        validation: CodexImportValidation::HostValidated,
        presence: CodexImportPresence::AuthOrNonblankConfig,
        classification: CodexImportClassification::SnapshotPayload(ProviderTableSyntax::TablesOnly),
    })
}

fn one(result: Result<NativeImportStep, NativeImportError>) -> NativeImportCandidate {
    match result.unwrap() {
        NativeImportStep::Ready { mut candidates } => {
            assert_eq!(candidates.len(), 1);
            candidates.remove(0)
        }
        other => panic!("unexpected import step: {other:?}"),
    }
}

#[test]
fn default_policy_preserves_strict_import_and_native_identity() {
    let adapter = builtin_app_adapter(&AppType::Codex);
    let policy = NativeImportPolicy::Codex(CodexImportPolicy::default());
    for (auth, config, official) in [
        (None, Some(""), false),
        (Some("{}"), None, false),
        (Some(r#"{"last_refresh":"yesterday"}"#), None, false),
        (Some(r#"{"tokens":{"access_token":"oauth"}}"#), None, true),
        (
            Some(r#"{"tokens":{"access_token":"oauth"}}"#),
            Some("experimental_bearer_token = 'key'"),
            true,
        ),
        (Some(r#"{"OPENAI_API_KEY":"key"}"#), None, false),
    ] {
        let docs = documents(auth, config);
        let expected = one(adapter.project_native_import(&docs));
        let actual = one(adapter.project_native_import_with_policy(&docs, &policy));
        assert_eq!(actual, expected);
        assert_eq!(
            actual.provider.id,
            if official {
                "codex-official"
            } else {
                "default"
            }
        );
        assert_eq!(
            actual.classification,
            Some(if official {
                NativeProviderMode::Official
            } else {
                NativeProviderMode::Custom
            })
        );
    }
    for (auth, config, target) in [
        (Some("null"), None, LogicalTarget::CodexAuth),
        (Some("[1]"), None, LogicalTarget::CodexAuth),
        (Some("{"), None, LogicalTarget::CodexAuth),
        (None, Some("invalid = ["), LogicalTarget::CodexConfig),
    ] {
        let docs = documents(auth, config);
        for result in [
            adapter.project_native_import(&docs),
            adapter.project_native_import_with_policy(&docs, &policy),
        ] {
            assert!(
                matches!(result, Err(NativeImportError::InvalidDocument { target: actual, .. }) if actual == target)
            );
        }
    }
}

#[test]
fn host_validated_snapshots_preserve_auth_shape_config_and_presence() {
    let adapter = builtin_app_adapter(&AppType::Codex);
    let policy = snapshot_policy();
    for config in [None, Some(""), Some("\u{a0}\n")] {
        assert!(matches!(
            adapter.project_native_import_with_policy(&documents(None, config), &policy),
            Err(NativeImportError::Missing { .. })
        ));
        for auth in [
            "null",
            "false",
            "42",
            "\"opaque\"",
            "[1,2]",
            "{\"future\":{\"keep\":true}}",
        ] {
            let imported =
                one(adapter
                    .project_native_import_with_policy(&documents(Some(auth), config), &policy));
            assert_eq!(
                imported.provider.settings,
                json!({"auth":serde_json::from_str::<serde_json::Value>(auth).unwrap(), "config":config.unwrap_or_default()})
            );
        }
    }
    let config = "# keep\nfuture = { unknown = [1, 2] }\n";
    let imported =
        one(adapter.project_native_import_with_policy(&documents(None, Some(config)), &policy));
    assert_eq!(
        imported.provider.settings,
        json!({"auth":{},"config":config})
    );
    assert!(matches!(
        adapter.project_native_import_with_policy(&documents(Some("{"), Some(config)), &policy),
        Err(NativeImportError::InvalidDocument {
            target: LogicalTarget::CodexAuth,
            ..
        })
    ));
    assert!(!format!("{imported:?}").contains("unknown"));
}

#[test]
fn snapshot_classification_uses_payload_and_selected_token_without_rewriting() {
    let adapter = builtin_app_adapter(&AppType::Codex);
    let policy = snapshot_policy();
    for (auth, config, official) in [
        (r#"{"last_refresh":"yesterday"}"#, "", true),
        (r#"{"tokens":{"unknown":"opaque"}}"#, "", true),
        (r#"{"tokens":{}}"#, "", false),
        (r#"{"tokens":{"access_token":"oauth"}}"#, "experimental_bearer_token = 'key'", false),
        (r#"{"OPENAI_API_KEY":" key ","tokens":{"access_token":"oauth"}}"#, "", false),
        (r#"{"last_refresh":"yesterday"}"#, "model_provider = 'v'\nmodel_providers = {v={experimental_bearer_token='inline'}}", true),
        (r#"{"last_refresh":"yesterday"}"#, "model_provider = 'v'\nexperimental_bearer_token = 'root'\n[model_providers.v]\nexperimental_bearer_token = ' '", true),
        (r#"{"last_refresh":"yesterday"}"#, "model_provider = 'OPENAI'\n[model_providers.OPENAI]\nexperimental_bearer_token = 'named'", true),
    ] {
        let imported = one(adapter.project_native_import_with_policy(&documents(Some(auth), Some(config)), &policy));
        assert_eq!(imported.classification, Some(if official { NativeProviderMode::Official } else { NativeProviderMode::Custom }), "{auth}: {config}");
        assert_eq!(imported.provider.settings["config"], config);
    }
    let inline = documents(
        Some(r#"{"last_refresh":"yesterday"}"#),
        Some("model_provider = 'v'\nmodel_providers = {v={experimental_bearer_token='inline'}}"),
    );
    let mut settings = match policy {
        NativeImportPolicy::Codex(settings) => settings,
        _ => panic!("expected Codex policy"),
    };
    settings.classification =
        CodexImportClassification::SnapshotPayload(ProviderTableSyntax::TablesAndInlineTables);
    let imported =
        one(adapter
            .project_native_import_with_policy(&inline, &NativeImportPolicy::Codex(settings)));
    assert_eq!(imported.classification, Some(NativeProviderMode::Custom));
}

#[test]
fn policy_dispatch_is_registry_bound_and_does_not_observe_catalogs() {
    let policy = snapshot_policy();
    for adapter in builtin_app_adapters() {
        let app = adapter.descriptor().app().clone();
        let docs = LiveDocumentSet::try_new(
            app.clone(),
            adapter
                .targets()
                .iter()
                .copied()
                .map(ObservedDocument::unobserved),
        )
        .unwrap();
        if app == AppType::Codex {
            assert!(matches!(
                adapter.project_native_import_with_policy(&docs, &policy),
                Ok(NativeImportStep::Observe {
                    target: LogicalTarget::CodexConfig
                })
            ));
        } else {
            assert!(matches!(
                adapter.project_native_import_with_policy(&docs, &policy),
                Err(NativeImportError::UnsupportedPolicy { .. })
            ));
        }
    }
    let adapter = builtin_app_adapter(&AppType::Codex);
    let wrong_app = LiveDocumentSet::try_new(
        AppType::Claude,
        [ObservedDocument::missing(LogicalTarget::ClaudeSettings)],
    )
    .unwrap();
    assert!(matches!(
        adapter.project_native_import_with_policy(&wrong_app, &policy),
        Err(NativeImportError::WrongDocumentApp { .. })
    ));
    let docs = LiveDocumentSet::try_new(
        AppType::Codex,
        [
            ObservedDocument::missing(LogicalTarget::CodexConfig),
            ObservedDocument::unobserved(LogicalTarget::CodexAuth),
            ObservedDocument::unobserved(LogicalTarget::CodexModelCatalog),
        ],
    )
    .unwrap();
    assert!(matches!(
        adapter.project_native_import_with_policy(&docs, &policy),
        Ok(NativeImportStep::Observe {
            target: LogicalTarget::CodexAuth
        })
    ));
}

#[test]
fn local_document_bounds_do_not_weaken_default_or_inventory_validation() {
    let config = format!("# {}\nmodel = 'x'", "x".repeat(MAX_OPERATION_CONTENT_BYTES));
    let observed = || {
        [
            ObservedDocument::missing(LogicalTarget::CodexAuth),
            ObservedDocument::present(LogicalTarget::CodexConfig, config.as_bytes()),
            ObservedDocument::unobserved(LogicalTarget::CodexModelCatalog),
        ]
    };
    assert!(matches!(
        LiveDocumentSet::try_new(AppType::Codex, observed()),
        Err(LiveDocumentSetError::ContentTooLarge { .. })
    ));
    assert!(matches!(
        LiveDocumentSet::try_new_with_content_limit(AppType::Codex, observed(), config.len() - 1),
        Err(LiveDocumentSetError::ContentTooLarge { .. })
    ));
    let docs =
        LiveDocumentSet::try_new_with_content_limit(AppType::Codex, observed(), config.len())
            .unwrap();
    let imported = one(builtin_app_adapter(&AppType::Codex)
        .project_native_import_with_policy(&docs, &snapshot_policy()));
    assert_eq!(imported.provider.settings["config"], config);
    assert!(matches!(
        LiveDocumentSet::try_new_with_content_limit(
            AppType::Codex,
            [ObservedDocument::missing(LogicalTarget::CodexAuth)],
            usize::MAX
        ),
        Err(LiveDocumentSetError::MissingTarget { .. })
    ));
}
