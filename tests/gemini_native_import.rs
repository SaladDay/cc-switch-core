use cc_switch_core::{
    builtin_app_adapter, builtin_app_adapters,
    gemini::{parse_env_assignments, EnvAssignmentErrorKind, EnvAssignmentSyntax},
    AppType, CodexImportPolicy, LiveDocumentSet, LogicalTarget, NativeImportCandidate,
    NativeImportError, NativeImportPolicy, NativeImportStep, NativeProviderMode, ObservedDocument,
};
use serde_json::{json, Value};

fn documents(env: Option<&str>, settings: Option<&str>) -> LiveDocumentSet {
    LiveDocumentSet::try_new(
        AppType::Gemini,
        [
            (LogicalTarget::GeminiEnv, env),
            (LogicalTarget::GeminiSettings, settings),
        ]
        .map(|(target, text)| {
            text.map_or_else(
                || ObservedDocument::missing(target),
                |text| ObservedDocument::present(target, text.as_bytes()),
            )
        }),
    )
    .unwrap()
}

fn one(result: Result<NativeImportStep, NativeImportError>) -> NativeImportCandidate {
    match result.unwrap() {
        NativeImportStep::Ready { mut candidates } => {
            assert_eq!(candidates.len(), 1);
            candidates.remove(0)
        }
        other => panic!("unexpected step: {other:?}"),
    }
}

#[test]
fn gemini_assignment_grammars_preserve_literals_order_and_redact_errors() {
    for syntax in [
        EnvAssignmentSyntax::Portable,
        EnvAssignmentSyntax::UnicodeLiteral,
    ] {
        let actual = parse_env_assignments(
            " # comment\r\n Z = 'literal' \r\nA=a=b # literal\nZ=last\n9_=\n",
            syntax,
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(
            actual,
            [
                ("Z", "'literal'"),
                ("A", "a=b # literal"),
                ("Z", "last"),
                ("9_", "")
            ]
        );
    }
    for (line, kind) in [
        (
            "secret-without-equals",
            EnvAssignmentErrorKind::MissingSeparator,
        ),
        (" =secret", EnvAssignmentErrorKind::EmptyKey),
        ("BAD-KEY=secret", EnvAssignmentErrorKind::InvalidKey),
        ("A=x\0secret", EnvAssignmentErrorKind::InvalidValue),
        ("A=x\rsecret", EnvAssignmentErrorKind::InvalidValue),
        ("变量=secret", EnvAssignmentErrorKind::InvalidKey),
    ] {
        let content = format!("# comment\n\n{line}");
        let error = parse_env_assignments(&content, EnvAssignmentSyntax::Portable)
            .next()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.line, 3);
        assert_eq!(error.kind, kind);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    let tolerant = parse_env_assignments(
        "invalid\n=skip\nBAD-KEY=skip\n变量=opaque\0value\nA=x\ry\n",
        EnvAssignmentSyntax::UnicodeLiteral,
    )
    .filter_map(Result::ok)
    .collect::<Vec<_>>();
    assert_eq!(tolerant, [("变量", "opaque\0value"), ("A", "x\ry")]);
}

#[test]
fn gemini_default_import_keeps_validation_identity_and_error_order() {
    let adapter = builtin_app_adapter(&AppType::Gemini);
    let custom =
        one(adapter.project_native_import(&documents(Some("Z=1\nGEMINI_API_KEY=fake\nZ=2"), None)));
    assert_eq!(custom.classification, Some(NativeProviderMode::Custom));
    assert_eq!(custom.provider.id, "default");
    assert_eq!(
        custom.provider.settings["env"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["Z", "GEMINI_API_KEY"]
    );
    let official = one(adapter.project_native_import(&documents(
        None,
        Some(r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#),
    )));
    assert_eq!(official.classification, Some(NativeProviderMode::Official));
    assert_eq!(official.provider.id, "gemini-official");
    for (env, settings, target, message) in [
        (
            "bad",
            "[]",
            LogicalTarget::GeminiSettings,
            "Gemini JSON root must be an object",
        ),
        (
            "bad",
            "{}",
            LogicalTarget::GeminiEnv,
            "Gemini .env line 1 has no '=' separator",
        ),
        (
            "变量=fake",
            "{}",
            LogicalTarget::GeminiEnv,
            "Gemini .env line 1 has an invalid variable name",
        ),
        (
            "A=x\0y",
            "{}",
            LogicalTarget::GeminiEnv,
            "Gemini .env line 1 has an invalid value",
        ),
    ] {
        match adapter
            .project_native_import(&documents(Some(env), Some(settings)))
            .unwrap_err()
        {
            NativeImportError::InvalidDocument {
                target: actual,
                message: actual_message,
            } => {
                assert_eq!(actual, target);
                assert_eq!(actual_message, message);
            }
            other => panic!("unexpected error: {other}"),
        }
    }
    assert!(adapter
        .project_native_import(&documents(Some("OTHER=value"), None))
        .is_err());
}

#[test]
fn gemini_snapshot_preserves_native_fields_without_classifying_or_validating_login() {
    let adapter = builtin_app_adapter(&AppType::Gemini);
    for settings in [
        "null",
        "false",
        "42",
        "\"opaque\"",
        "[1,2]",
        r#"{"security":{"auth":{"selectedType":"oauth-personal","future":true}},"mcpServers":{"tool":{"command":"fake"}},"advanced":{"opaque":[1,2]}}"#,
    ] {
        let imported = one(adapter.project_native_import_with_policy(
            &documents(
                Some("invalid\n变量=opaque\0value\nA=first\nA=last"),
                Some(settings),
            ),
            &NativeImportPolicy::GeminiEnvSnapshot,
        ));
        assert_eq!(imported.classification, None);
        assert_eq!(
            imported.provider.settings,
            json!({
                "env":{"变量":"opaque\0value","A":"last"},
                "config":serde_json::from_str::<Value>(settings).unwrap()
            })
        );
        assert!(!format!("{imported:?}").contains("opaque"));
    }
    let imported = one(adapter.project_native_import_with_policy(
        &documents(Some(""), None),
        &NativeImportPolicy::GeminiEnvSnapshot,
    ));
    assert_eq!(imported.provider.settings, json!({"env":{},"config":{}}));
    assert!(matches!(
        adapter.project_native_import_with_policy(
            &documents(Some(""), Some("{")),
            &NativeImportPolicy::GeminiEnvSnapshot
        ),
        Err(NativeImportError::InvalidDocument {
            target: LogicalTarget::GeminiSettings,
            ..
        })
    ));
}

#[test]
fn gemini_snapshot_observes_only_required_targets_and_rejects_other_policies() {
    let adapter = builtin_app_adapter(&AppType::Gemini);
    let make = |env| {
        LiveDocumentSet::try_new(
            AppType::Gemini,
            [
                env,
                ObservedDocument::unobserved(LogicalTarget::GeminiSettings),
            ],
        )
        .unwrap()
    };
    let policy = NativeImportPolicy::GeminiEnvSnapshot;
    assert_eq!(
        adapter
            .project_native_import_with_policy(
                &make(ObservedDocument::unobserved(LogicalTarget::GeminiEnv)),
                &policy
            )
            .unwrap(),
        NativeImportStep::Observe {
            target: LogicalTarget::GeminiEnv
        }
    );
    assert!(matches!(
        adapter.project_native_import_with_policy(
            &make(ObservedDocument::missing(LogicalTarget::GeminiEnv)),
            &policy
        ),
        Err(NativeImportError::Missing { .. })
    ));
    assert_eq!(
        adapter
            .project_native_import_with_policy(
                &make(ObservedDocument::present(LogicalTarget::GeminiEnv, b"")),
                &policy
            )
            .unwrap(),
        NativeImportStep::Observe {
            target: LogicalTarget::GeminiSettings
        }
    );
    assert!(matches!(
        adapter.project_native_import_with_policy(
            &documents(Some(""), None),
            &NativeImportPolicy::Codex(CodexImportPolicy::default())
        ),
        Err(NativeImportError::UnsupportedPolicy { .. })
    ));
    for other in builtin_app_adapters().filter(|other| other.descriptor().app() != &AppType::Gemini)
    {
        let docs = LiveDocumentSet::try_new(
            other.descriptor().app().clone(),
            other
                .targets()
                .iter()
                .copied()
                .map(ObservedDocument::unobserved),
        )
        .unwrap();
        assert!(matches!(
            other.project_native_import_with_policy(&docs, &policy),
            Err(NativeImportError::UnsupportedPolicy { .. })
        ));
        assert!(matches!(
            adapter.project_native_import_with_policy(&docs, &policy),
            Err(NativeImportError::WrongDocumentApp { .. })
        ));
    }
}
