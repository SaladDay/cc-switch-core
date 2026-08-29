//! Pure projection of CC Switch common configuration snippets.

use serde_json::Value;
use thiserror::Error;
use toml_edit::{DocumentMut, Item, TableLike, Value as TomlValue};

use crate::AppType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ApplyCommonConfigError {
    #[error("common configuration must be valid JSON")]
    InvalidJson,
    #[error("common configuration must be a JSON object")]
    JsonNotObject,
    #[error("provider settings must be a JSON object")]
    SettingsNotObject,
    #[error("Codex provider config must be a TOML string")]
    ConfigNotString,
    #[error("Codex provider config must be valid TOML")]
    InvalidCodexConfig,
    #[error("Codex common configuration must be valid TOML")]
    InvalidCodexSnippet,
    #[error("Claude common configuration contains a provider-specific or credential key")]
    ClaudeForbiddenKey,
    #[error("Claude common configuration env must be a JSON object")]
    ClaudeEnvNotObject,
    #[error("Codex common configuration contains a provider-specific or credential key")]
    CodexForbiddenKey,
    #[error("Gemini common configuration contains a provider endpoint or credential key")]
    GeminiForbiddenKey,
    #[error("Gemini common configuration values must be strings")]
    GeminiValueNotString,
}

/// Applies a shared-database common configuration snippet to one provider.
///
/// Only Claude, Codex, and Gemini have common snippets in CC Switch. The
/// caller owns the enablement decision from provider metadata; disabled or
/// empty snippets are returned unchanged.
pub fn apply(
    app: &AppType,
    settings: &Value,
    snippet: Option<&str>,
    enabled: bool,
) -> Result<Value, ApplyCommonConfigError> {
    let Some(snippet) = snippet.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(settings.clone());
    };
    if !enabled {
        return Ok(settings.clone());
    }

    match app {
        AppType::Claude => {
            let source: Value =
                serde_json::from_str(snippet).map_err(|_| ApplyCommonConfigError::InvalidJson)?;
            if !source.is_object() {
                return Err(ApplyCommonConfigError::JsonNotObject);
            }
            validate_claude_snippet(&source)?;
            let mut result = settings.clone();
            if !result.is_object() {
                return Err(ApplyCommonConfigError::SettingsNotObject);
            }
            json_deep_merge(&mut result, &source);
            Ok(result)
        }
        AppType::Codex => {
            let mut result = settings.clone();
            let object = result
                .as_object_mut()
                .ok_or(ApplyCommonConfigError::SettingsNotObject)?;
            let config = match object.get("config") {
                Some(Value::String(config)) => config.as_str(),
                Some(Value::Null) | None => "",
                Some(_) => return Err(ApplyCommonConfigError::ConfigNotString),
            };
            let mut target = if config.trim().is_empty() {
                DocumentMut::new()
            } else {
                config
                    .parse::<DocumentMut>()
                    .map_err(|_| ApplyCommonConfigError::InvalidCodexConfig)?
            };
            let source = snippet
                .parse::<DocumentMut>()
                .map_err(|_| ApplyCommonConfigError::InvalidCodexSnippet)?;
            validate_codex_snippet(&source)?;
            merge_toml_table_like(target.as_table_mut(), source.as_table());
            object.insert("config".to_owned(), Value::String(target.to_string()));
            Ok(result)
        }
        AppType::Gemini => {
            let source: Value =
                serde_json::from_str(snippet).map_err(|_| ApplyCommonConfigError::InvalidJson)?;
            if !source.is_object() {
                return Err(ApplyCommonConfigError::JsonNotObject);
            }
            for (key, value) in source.as_object().expect("object checked above") {
                if is_gemini_route_key(key) || is_sensitive_config_key(key) {
                    return Err(ApplyCommonConfigError::GeminiForbiddenKey);
                }
                if !value.is_string() {
                    return Err(ApplyCommonConfigError::GeminiValueNotString);
                }
            }
            let object = settings
                .as_object()
                .ok_or(ApplyCommonConfigError::SettingsNotObject)?;
            let mut result = Value::Object(object.clone());
            if let Some(env) = result.get_mut("env") {
                if !env.is_object() {
                    return Err(ApplyCommonConfigError::SettingsNotObject);
                }
                json_deep_merge(env, &source);
            } else if let Some(result) = result.as_object_mut() {
                result.insert("env".to_owned(), source);
            }
            Ok(result)
        }
        AppType::GrokBuild
        | AppType::OpenCode
        | AppType::OpenClaw
        | AppType::ClaudeDesktop
        | AppType::Hermes
        | AppType::Pi => Ok(settings.clone()),
    }
}

fn validate_claude_snippet(source: &Value) -> Result<(), ApplyCommonConfigError> {
    const TOP_LEVEL_FORBIDDEN: &[&str] = &["apiBaseUrl", "primaryModel", "smallFastModel"];

    let object = source
        .as_object()
        .ok_or(ApplyCommonConfigError::JsonNotObject)?;
    if json_contains_sensitive_key(source)
        || object
            .keys()
            .any(|key| TOP_LEVEL_FORBIDDEN.contains(&key.as_str()))
    {
        return Err(ApplyCommonConfigError::ClaudeForbiddenKey);
    }
    let Some(env) = object.get("env") else {
        return Ok(());
    };
    let env = env
        .as_object()
        .ok_or(ApplyCommonConfigError::ClaudeEnvNotObject)?;
    if env.keys().any(|key| is_claude_route_key(key)) {
        return Err(ApplyCommonConfigError::ClaudeForbiddenKey);
    }
    Ok(())
}

fn validate_codex_snippet(source: &DocumentMut) -> Result<(), ApplyCommonConfigError> {
    const FORBIDDEN: &[&str] = &[
        "model",
        "model_provider",
        "base_url",
        "wire_api",
        "model_providers",
        "experimental_bearer_token",
        "model_catalog_json",
        "mcp_servers",
        "profile",
        "profiles",
        "review_model",
    ];
    let root = source.as_table();
    if root.iter().any(|(key, _)| FORBIDDEN.contains(&key))
        || root.iter().any(|(key, item)| {
            is_sensitive_config_key(key) || toml_item_contains_sensitive_key(item)
        })
        || root
            .get("mcp")
            .and_then(Item::as_table_like)
            .is_some_and(|mcp| mcp.contains_key("servers"))
        || root.get("web_search").and_then(Item::as_str) == Some("disabled")
    {
        return Err(ApplyCommonConfigError::CodexForbiddenKey);
    }
    Ok(())
}

fn is_claude_route_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_BASE_URL")
        || upper == "ENDPOINT_ID"
        || upper.starts_with("CLAUDE_CODE_USE_")
        || upper.starts_with("AWS_")
        || upper.starts_with("GOOGLE_")
        || upper.contains("VERTEX")
        || upper.contains("FOUNDRY")
        || ((upper.starts_with("ANTHROPIC_") || upper.starts_with("CLAUDE_CODE_"))
            && (upper.contains("MODEL")
                || matches!(
                    upper.as_str(),
                    "CLAUDE_CODE_MAX_CONTEXT_TOKENS" | "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
                )))
}

fn is_gemini_route_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "GEMINI_MODEL"
        || upper.ends_with("_BASE_URL")
        || upper == "GOOGLE_GENAI_USE_VERTEXAI"
        || upper.starts_with("GOOGLE_CLOUD_")
        || upper.contains("VERTEX")
}

fn json_contains_sensitive_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| is_sensitive_config_key(key) || json_contains_sensitive_key(value)),
        Value::Array(array) => array.iter().any(json_contains_sensitive_key),
        _ => false,
    }
}

fn toml_item_contains_sensitive_key(item: &Item) -> bool {
    if let Some(table) = item.as_table_like() {
        return table.iter().any(|(key, item)| {
            is_sensitive_config_key(key) || toml_item_contains_sensitive_key(item)
        });
    }
    if let Some(tables) = item.as_array_of_tables() {
        return tables.iter().any(|table| {
            table.iter().any(|(key, item)| {
                is_sensitive_config_key(key) || toml_item_contains_sensitive_key(item)
            })
        });
    }
    item.as_value()
        .is_some_and(toml_value_contains_sensitive_key)
}

fn toml_value_contains_sensitive_key(value: &TomlValue) -> bool {
    match value {
        TomlValue::InlineTable(table) => table.iter().any(|(key, value)| {
            is_sensitive_config_key(key) || toml_value_contains_sensitive_key(value)
        }),
        TomlValue::Array(array) => array.iter().any(toml_value_contains_sensitive_key),
        _ => false,
    }
}

fn is_sensitive_config_key(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "APIKEY",
        "API_KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIALS",
        "AUTHORIZATION",
        "COOKIE",
    ];
    const SUFFIXES: &[&str] = &[
        "_KEY",
        "_API_KEY",
        "_ACCESS_KEY",
        "_ACCESS_KEY_ID",
        "_KEY_ID",
        "_PRIVATE_KEY",
        "_APIKEY",
        "_ACCESSKEY",
        "_SECRETKEY",
        "_APITOKEN",
        "_AUTH_TOKEN",
        "_TOKEN",
        "_PAT",
        "_PWD",
        "_PASS",
        "_PASSPHRASE",
        "_CREDS",
        "_HEADERS",
        "_HEADER",
    ];
    const CONTAINS: &[&str] = &[
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE_KEY",
        "BEARER_TOKEN",
    ];
    let upper = name.to_ascii_uppercase();
    EXACT.contains(&upper.as_str())
        || SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
        || CONTAINS.iter().any(|part| upper.contains(part))
}

fn json_deep_merge(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, source_value) in source {
                match target.get_mut(key) {
                    Some(target_value) => json_deep_merge(target_value, source_value),
                    None => {
                        target.insert(key.clone(), source_value.clone());
                    }
                }
            }
        }
        (target, source) => *target = source.clone(),
    }
}

fn merge_toml_item(target: &mut Item, source: &Item) {
    if let Some(source_table) = source.as_table_like() {
        if let Some(target_table) = target.as_table_like_mut() {
            merge_toml_table_like(target_table, source_table);
            return;
        }
    }
    *target = source.clone();
}

fn merge_toml_table_like(target: &mut dyn TableLike, source: &dyn TableLike) {
    for (key, source_item) in source.iter() {
        match target.get_mut(key) {
            Some(target_item) => merge_toml_item(target_item, source_item),
            None => {
                target.insert(key, source_item.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_only_enabled_supported_snippets() {
        let claude = apply(
            &AppType::Claude,
            &json!({"env": {"KEEP": "provider"}, "keep": true}),
            Some(r#"{"env":{"KEEP":"common","EXTRA":"yes"}}"#),
            true,
        )
        .expect("valid common config");
        assert_eq!(claude["env"]["KEEP"], "common");
        assert_eq!(claude["env"]["EXTRA"], "yes");
        assert_eq!(claude["keep"], true);

        let unchanged = apply(
            &AppType::Claude,
            &json!({"keep": true}),
            Some(r#"{"added":true}"#),
            false,
        )
        .expect("disabled snippet");
        assert_eq!(unchanged, json!({"keep": true}));
    }

    #[test]
    fn merges_codex_toml_without_dropping_provider_fields() {
        let result = apply(
            &AppType::Codex,
            &json!({"auth": {}, "config": "model = \"provider\"\n[features]\na = true\n"}),
            Some("[features]\nb = true\n"),
            true,
        )
        .expect("valid TOML");
        let config = result["config"].as_str().expect("config text");
        let parsed = config.parse::<toml_edit::DocumentMut>().expect("TOML");
        assert_eq!(parsed["model"].as_str(), Some("provider"));
        assert_eq!(parsed["features"]["a"].as_bool(), Some(true));
        assert_eq!(parsed["features"]["b"].as_bool(), Some(true));
    }

    #[test]
    fn merges_gemini_snippet_into_env_only() {
        let result = apply(
            &AppType::Gemini,
            &json!({"env": {"GEMINI_MODEL": "provider"}, "settings": {"keep": true}}),
            Some(r#"{"HTTPS_PROXY":"http://127.0.0.1:8080"}"#),
            true,
        )
        .expect("valid common config");
        assert_eq!(result["env"]["GEMINI_MODEL"], "provider");
        assert_eq!(result["env"]["HTTPS_PROXY"], "http://127.0.0.1:8080");
        assert_eq!(result["settings"]["keep"], true);
    }

    #[test]
    fn rejects_non_string_codex_config_instead_of_overwriting_it() {
        assert_eq!(
            apply(
                &AppType::Codex,
                &json!({"auth": {}, "config": {"future": true}}),
                Some("model = \"new\""),
                true,
            ),
            Err(ApplyCommonConfigError::ConfigNotString)
        );
    }

    #[test]
    fn rejects_gemini_endpoints_credentials_and_non_string_values() {
        for snippet in [
            r#"{"GOOGLE_GEMINI_BASE_URL":"https://attacker.example"}"#,
            r#"{"GOOGLE_API_KEY":"secret"}"#,
        ] {
            assert_eq!(
                apply(&AppType::Gemini, &json!({"env": {}}), Some(snippet), true),
                Err(ApplyCommonConfigError::GeminiForbiddenKey)
            );
        }
        assert_eq!(
            apply(
                &AppType::Gemini,
                &json!({"env": {}}),
                Some(r#"{"HTTPS_PROXY":42}"#),
                true,
            ),
            Err(ApplyCommonConfigError::GeminiValueNotString)
        );
    }

    #[test]
    fn rejects_claude_provider_fields_and_credentials() {
        for snippet in [
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://attacker.example"}}"#,
            r#"{"env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"other"}}"#,
            r#"{"env":{"ANTHROPIC_SMALL_FAST_MODEL":"other"}}"#,
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#,
            r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"Authorization: Bearer secret"}}"#,
            r#"{"hooks":[{"env":{"OPENAI_API_KEY":"secret"}}]}"#,
            r#"{"env":{"OPENROUTER_API_KEY":"secret"}}"#,
            r#"{"apiKey":"secret"}"#,
        ] {
            assert_eq!(
                apply(&AppType::Claude, &json!({"env": {}}), Some(snippet), true),
                Err(ApplyCommonConfigError::ClaudeForbiddenKey)
            );
        }
        assert_eq!(
            apply(
                &AppType::Claude,
                &json!({"env": {}}),
                Some(r#"{"env":"invalid"}"#),
                true,
            ),
            Err(ApplyCommonConfigError::ClaudeEnvNotObject)
        );
    }

    #[test]
    fn rejects_codex_provider_fields_credentials_and_owned_artifacts() {
        for snippet in [
            "model_provider = \"attacker\"\n",
            "experimental_bearer_token = \"secret\"\n",
            "web_search = \"disabled\"\n",
            "[model_providers.attacker]\nbase_url = \"https://attacker.example\"\n",
            "profile = \"attacker\"\n",
            "[profiles.attacker]\nmodel_provider = \"attacker\"\n",
            "review_model = \"attacker\"\n",
            "[mcp.servers.attacker]\ncommand = \"run\"\n",
            "[shell_environment_policy.set]\nOPENAI_API_KEY = \"secret\"\n",
            "rules = [{ env = { AUTHORIZATION = \"Bearer secret\" } }]\n",
        ] {
            assert_eq!(
                apply(
                    &AppType::Codex,
                    &json!({"auth": {}, "config": ""}),
                    Some(snippet),
                    true,
                ),
                Err(ApplyCommonConfigError::CodexForbiddenKey)
            );
        }
    }

    #[test]
    fn rejects_gemini_model_selection() {
        for snippet in [
            r#"{"GEMINI_MODEL":"other"}"#,
            r#"{"GOOGLE_GENAI_USE_VERTEXAI":"true"}"#,
            r#"{"GOOGLE_CLOUD_PROJECT":"other"}"#,
            r#"{"GOOGLE_CLOUD_LOCATION":"us-east1"}"#,
        ] {
            assert_eq!(
                apply(
                    &AppType::Gemini,
                    &json!({"env": {"GEMINI_MODEL": "provider"}}),
                    Some(snippet),
                    true,
                ),
                Err(ApplyCommonConfigError::GeminiForbiddenKey)
            );
        }
    }
}
