use cc_switch_core::codex::{
    extract_api_key, prepare_provider_live_config, prepare_provider_live_config_with_syntax,
    read_experimental_bearer_token, remove_experimental_bearer_token_if,
    restore_provider_token_for_backfill, sanitize_third_party_auth, set_experimental_bearer_token,
    PrepareNativeLiveError, ProviderTableSyntax,
};
use serde_json::json;
use toml_edit::DocumentMut;

#[test]
fn token_reads_preserve_precedence_and_host_table_syntax() {
    for (source, tables_only, all_tables) in [
        ("", None, None),
        ("experimental_bearer_token = ' root '", Some("root"), Some("root")),
        ("model_provider = ' vendor '\nexperimental_bearer_token = 'root'\n[model_providers.vendor]\nexperimental_bearer_token = ' scoped '", Some("scoped"), Some("scoped")),
        ("model_provider = 'vendor'\nexperimental_bearer_token = 'root'\n[model_providers.vendor]\nexperimental_bearer_token = ' '", None, None),
        ("model_provider = 'vendor'\nexperimental_bearer_token = 'root'\n[model_providers.vendor]\nexperimental_bearer_token = 1", Some("root"), Some("root")),
        ("model_provider = 'vendor'\nexperimental_bearer_token = 'root'\n[model_providers]\nvendor = {experimental_bearer_token = 'inline'}", Some("root"), Some("inline")),
        ("model_provider = 'vendor'\nexperimental_bearer_token = 'root'\nmodel_providers = {vendor = {experimental_bearer_token = 'inline'}}", Some("root"), Some("inline")),
        ("model_provider = 'vendor'\nmodel_providers = {vendor = {experimental_bearer_token = 'inline'}}", None, Some("inline")),
        ("model_provider = 'vendor'\nexperimental_bearer_token = 'root'\nmodel_providers = []", Some("root"), Some("root")),
        ("model_provider = 'missing'\n[model_providers.vendor]\nexperimental_bearer_token = 'inactive'", None, None),
        ("model_provider = ' '\nexperimental_bearer_token = 'root'", Some("root"), Some("root")),
        ("model_provider = 1\nexperimental_bearer_token = 'root'", Some("root"), Some("root")),
    ] {
        assert_eq!(read_experimental_bearer_token(source, ProviderTableSyntax::TablesOnly).as_deref(), tables_only, "{source}");
        assert_eq!(read_experimental_bearer_token(source, ProviderTableSyntax::TablesAndInlineTables).as_deref(), all_tables, "{source}");
    }
}

#[test]
fn credential_selection_and_sanitization_preserve_source_precedence() {
    let config = "experimental_bearer_token = ' live-config '\n";
    let fallback = json!({"OPENAI_API_KEY": "stored-auth", "tokens": {"access_token":"not-a-key"}});
    for (auth, config, expected) in [
        (
            json!({"OPENAI_API_KEY":" live-auth ", "tokens": {"access_token":"oauth"}}),
            Some(config),
            Some("live-auth"),
        ),
        (
            json!({"OPENAI_API_KEY":" ", "auth_mode":"chatgpt"}),
            Some(config),
            Some("live-config"),
        ),
        (
            json!({"OPENAI_API_KEY":false}),
            Some(config),
            Some("live-config"),
        ),
        (json!(null), None, None),
        (json!(["opaque"]), Some("invalid = ["), None),
    ] {
        let syntax = ProviderTableSyntax::TablesOnly;
        assert_eq!(
            extract_api_key(Some(&auth), config, syntax).as_deref(),
            expected
        );
        assert_eq!(
            sanitize_third_party_auth(
                Some(&auth),
                config,
                Some(&fallback),
                Some("experimental_bearer_token = 'stored-config'"),
                syntax
            ),
            json!({"OPENAI_API_KEY":expected.unwrap_or("stored-auth")})
        );
    }
    assert_eq!(
        sanitize_third_party_auth(None, None, None, None, ProviderTableSyntax::TablesOnly),
        json!({})
    );
    let inline = "model_provider = 'vendor'\nmodel_providers = {vendor = {experimental_bearer_token = 'inline'}}";
    assert_eq!(
        sanitize_third_party_auth(
            None,
            Some(inline),
            Some(&fallback),
            None,
            ProviderTableSyntax::TablesOnly
        ),
        json!({"OPENAI_API_KEY":"stored-auth"})
    );
    assert_eq!(
        sanitize_third_party_auth(
            None,
            Some(inline),
            Some(&fallback),
            None,
            ProviderTableSyntax::TablesAndInlineTables
        ),
        json!({"OPENAI_API_KEY":"inline"})
    );
    assert_eq!(
        prepare_provider_live_config_with_syntax(
            &json!({}),
            inline,
            ProviderTableSyntax::TablesOnly
        )
        .unwrap(),
        inline
    );
}

#[test]
fn token_removal_keeps_other_entries_and_predicate_order() {
    for id in ["vendor", "OPENAI"] {
        let source = format!("# header\nmodel_provider = ' {id} '\nexperimental_bearer_token = ' root '\n[model_providers.{id}]\nexperimental_bearer_token = ' scoped '\nfuture = [1, 2] # keep\n[model_providers.other]\nexperimental_bearer_token = 'inactive'\n");
        let observed = std::cell::RefCell::new(Vec::new());
        let cleaned = remove_experimental_bearer_token_if(
            &source,
            ProviderTableSyntax::TablesOnly,
            |token| {
                observed.borrow_mut().push(token.to_owned());
                token == "scoped"
            },
        )
        .unwrap();
        assert_eq!(*observed.borrow(), ["scoped", "root"]);
        let mut expected = source.parse::<DocumentMut>().unwrap();
        expected["model_providers"][id]
            .as_table_mut()
            .unwrap()
            .remove("experimental_bearer_token");
        assert_eq!(cleaned, expected.to_string());
    }
    for source in ["", "invalid = [", "  "] {
        assert_eq!(
            remove_experimental_bearer_token_if(
                source,
                ProviderTableSyntax::TablesOnly,
                |_| panic!("no token")
            )
            .unwrap(),
            source
        );
    }
    assert!(matches!(
        remove_experimental_bearer_token_if(
            "experimental_bearer_token = [",
            ProviderTableSyntax::TablesOnly,
            |_| true
        ),
        Err(PrepareNativeLiveError::InvalidConfig)
    ));
    let blank = "experimental_bearer_token = ' '\n[model_providers.inactive]\nexperimental_bearer_token = 'keep'\n";
    assert!(!remove_experimental_bearer_token_if(
        blank,
        ProviderTableSyntax::TablesOnly,
        str::is_empty
    )
    .unwrap()
    .starts_with("experimental_bearer_token"));
}

#[test]
fn token_removal_respects_ordinary_and_inline_table_boundaries() {
    for source in [
        "model_provider = 'vendor'\nmodel_providers = {vendor = {experimental_bearer_token = 'inline', other = true}}\n",
        "model_provider = 'vendor'\n[model_providers]\nvendor = {experimental_bearer_token = 'inline', other = true}\n",
    ] {
        assert_eq!(remove_experimental_bearer_token_if(source, ProviderTableSyntax::TablesOnly, |_| true).unwrap(), source);
        let cleaned = remove_experimental_bearer_token_if(source, ProviderTableSyntax::TablesAndInlineTables, |_| true).unwrap();
        let parsed = cleaned.parse::<DocumentMut>().unwrap();
        assert!(!parsed["model_providers"]["vendor"].as_table_like().unwrap().contains_key("experimental_bearer_token"));
        assert_eq!(parsed["model_providers"]["vendor"]["other"].as_bool(), Some(true));
    }
}

#[test]
fn snapshot_backfill_lifts_only_selected_token_and_uses_template_auth() {
    let source = "model_provider = 'vendor'\nexperimental_bearer_token = 'root'\n[model_providers.vendor]\nexperimental_bearer_token = 'selected'\n[model_providers.other]\nexperimental_bearer_token = 'inactive'\n";
    let template =
        json!({"auth":{"OPENAI_API_KEY":"old", "future":{"keep":true}}, "config":"ignored"});
    let mut live = json!({"auth":{"tokens":{"access_token":"live-oauth"}},"config":source,"modelCatalog":{"models":[]},"future":42});
    restore_provider_token_for_backfill(&mut live, &template, ProviderTableSyntax::TablesOnly)
        .unwrap();
    assert_eq!(
        live["auth"],
        json!({"OPENAI_API_KEY":"selected", "future":{"keep":true}})
    );
    assert_eq!(live["future"], 42);
    assert_eq!(live["modelCatalog"], json!({"models":[]}));
    let document = live["config"]
        .as_str()
        .unwrap()
        .parse::<DocumentMut>()
        .unwrap();
    assert!(document.get("experimental_bearer_token").is_none());
    assert!(document["model_providers"]["vendor"]
        .get("experimental_bearer_token")
        .is_none());
    assert_eq!(
        document["model_providers"]["other"]["experimental_bearer_token"].as_str(),
        Some("inactive")
    );
    for template in [json!({}), json!({"auth":null}), json!({"auth":42})] {
        let mut settings = json!({"config": "experimental_bearer_token = 'key'"});
        restore_provider_token_for_backfill(
            &mut settings,
            &template,
            ProviderTableSyntax::TablesOnly,
        )
        .unwrap();
        assert_eq!(settings["auth"], json!({"OPENAI_API_KEY":"key"}));
    }
    for mut settings in [
        json!(null),
        json!({"config":false}),
        json!({"config":"invalid = ["}),
        json!({"config":"model = 'x'", "auth":{"opaque":true}}),
    ] {
        let original = settings.clone();
        restore_provider_token_for_backfill(
            &mut settings,
            &template,
            ProviderTableSyntax::TablesOnly,
        )
        .unwrap();
        assert_eq!(settings, original);
    }
}

#[test]
fn reserved_provider_tables_are_never_selected_for_tokens() {
    for id in [
        "amazon-bedrock",
        "openai",
        "ollama",
        "lmstudio",
        "oss",
        "ollama-chat",
    ] {
        let id = id.to_ascii_uppercase();
        let source = format!("model_provider = ' {id} '\nexperimental_bearer_token = 'root'\n[model_providers.{id}]\nexperimental_bearer_token = 'private-to-builtin'\n");
        for syntax in [
            ProviderTableSyntax::TablesOnly,
            ProviderTableSyntax::TablesAndInlineTables,
        ] {
            assert_eq!(
                read_experimental_bearer_token(&source, syntax).as_deref(),
                Some("root")
            );
        }
        let result = set_experimental_bearer_token(&source, "replacement").expect("write token");
        let document = result.parse::<DocumentMut>().expect("TOML");
        assert_eq!(
            document["experimental_bearer_token"].as_str(),
            Some("replacement")
        );
        assert_eq!(
            document["model_providers"][&id]["experimental_bearer_token"].as_str(),
            Some("private-to-builtin")
        );
    }
}

#[test]
fn token_writes_change_only_the_selected_field() {
    for (source, scoped) in [
        ("# header\nmodel_provider = ' vendor '\nexperimental_bearer_token = 'root'\n[model_providers.vendor]\nbase_url = 'https://example.test' # retain\nexperimental_bearer_token = 'old'\n[model_providers.other]\nfuture = [1, 2]\n", true),
        ("model_provider = 'vendor'\n[model_providers.vendor]\nfuture = {nested = true}\n", true),
        ("model_provider = 'missing'\n[model_providers.other]\nfuture = true\n", false),
        ("model_provider = 'vendor'\n[model_providers]\nvendor = {experimental_bearer_token = 'inline', future = true}\n", false),
        ("model_provider = 'vendor'\nmodel_providers = {vendor = {future = true}}\n", false),
        ("model_provider = 'vendor'\nmodel_providers = 42\n", false),
        ("model = 'example'\n", false),
    ] {
        let mut expected = source.parse::<DocumentMut>().expect("fixture TOML");
        let token = "quotes\" and\nnewlines";
        if scoped {
            expected["model_providers"]["vendor"]["experimental_bearer_token"] = toml_edit::value(token);
        } else {
            expected["experimental_bearer_token"] = toml_edit::value(token);
        }
        let result = set_experimental_bearer_token(source, token).expect("write token");
        assert_eq!(result, expected.to_string());
        assert_eq!(prepare_provider_live_config(&json!({"OPENAI_API_KEY":token}), source).expect("prepare config"), expected.to_string());
    }
}

#[test]
fn native_preparation_keeps_inline_fallback_and_validation_order() {
    let source = "model_provider = 'vendor'\nmodel_providers = {vendor = {experimental_bearer_token = ' inline '}}\n";
    let result = prepare_provider_live_config(&json!({}), source).expect("prepare config");
    let document = result.parse::<DocumentMut>().expect("TOML");
    assert_eq!(
        document["experimental_bearer_token"].as_str(),
        Some("inline")
    );
    assert_eq!(
        document["model_providers"]["vendor"]["experimental_bearer_token"].as_str(),
        Some(" inline ")
    );
    assert_eq!(
        prepare_provider_live_config(&json!({}), "invalid = [").expect("no token, no rewrite"),
        "invalid = ["
    );
    assert!(matches!(
        prepare_provider_live_config(&json!({"OPENAI_API_KEY":"key"}), "invalid = ["),
        Err(PrepareNativeLiveError::InvalidConfig)
    ));
    assert!(matches!(
        prepare_provider_live_config(&json!({"OPENAI_API_KEY":"key"}), " "),
        Err(PrepareNativeLiveError::MissingConfigForApiKey)
    ));
}
