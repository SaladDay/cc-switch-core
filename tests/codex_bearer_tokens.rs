use cc_switch_core::codex::{
    prepare_provider_live_config, read_experimental_bearer_token, set_experimental_bearer_token,
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
