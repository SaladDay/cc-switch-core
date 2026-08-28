//! Pure projection of CC Switch common configuration snippets.

use serde_json::Value;
use thiserror::Error;
use toml_edit::{DocumentMut, Item, TableLike};

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
                if key == "GOOGLE_GEMINI_BASE_URL" || is_sensitive_config_key(key) {
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

fn is_sensitive_config_key(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "APIKEY",
        "API_KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIALS",
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
            &json!({"env": {"TOKEN": "provider"}, "keep": true}),
            Some(r#"{"env":{"TOKEN":"common","EXTRA":"yes"}}"#),
            true,
        )
        .expect("valid common config");
        assert_eq!(claude["env"]["TOKEN"], "common");
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
                Some(r#"{"GEMINI_MODEL":42}"#),
                true,
            ),
            Err(ApplyCommonConfigError::GeminiValueNotString)
        );
    }
}
