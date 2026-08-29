//! Gemini live-configuration projection.

use std::{collections::BTreeMap, fmt};

use serde_json::{Map, Value};
use thiserror::Error;

/// Authentication selection written to Gemini's `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    OAuthPersonal,
}

impl AuthMode {
    fn selected_type(self) -> &'static str {
        match self {
            Self::ApiKey => "gemini-api-key",
            Self::OAuthPersonal => "oauth-personal",
        }
    }
}

/// Owned values required by a Gemini live writer.
#[derive(Clone, PartialEq)]
pub struct PreparedLiveSnapshot {
    pub env: BTreeMap<String, String>,
    pub settings: Value,
}

impl fmt::Debug for PreparedLiveSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLiveSnapshot")
            .field("env", &"<redacted>")
            .field("settings", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PrepareLiveSnapshotError {
    #[error("Gemini settings must be a JSON object")]
    SettingsNotObject,
    #[error("Gemini 'env' must be a JSON object")]
    EnvNotObject,
    #[error("Gemini environment variable values must be strings")]
    EnvValueNotString,
    #[error("Gemini 'config' must be a JSON object or null")]
    ConfigNotObject,
    #[error("existing Gemini settings must be a JSON object")]
    ExistingSettingsNotObject,
    #[error("Gemini settings field '{0}' must be a JSON object")]
    SettingsFieldNotObject(&'static str),
    #[error("Gemini API-key mode requires a non-empty GEMINI_API_KEY")]
    MissingApiKey,
}

/// Projects native provider settings over an optional existing
/// `settings.json` snapshot.
///
/// The caller remains responsible for reading and writing files. Provider
/// `config` values replace only matching top-level settings keys, while absent
/// or null `config` preserves the supplied existing snapshot.
pub fn prepare_live_snapshot(
    provider_settings: &Value,
    existing_settings: Option<&Value>,
    auth_mode: AuthMode,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    let provider = provider_settings
        .as_object()
        .ok_or(PrepareLiveSnapshotError::SettingsNotObject)?;
    let env = project_env(provider.get("env"))?;
    if auth_mode == AuthMode::ApiKey
        && env
            .get("GEMINI_API_KEY")
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(PrepareLiveSnapshotError::MissingApiKey);
    }

    let mut settings = match existing_settings {
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err(PrepareLiveSnapshotError::ExistingSettingsNotObject),
        None => Value::Object(Map::new()),
    };
    match provider.get("config") {
        Some(Value::Object(config)) => {
            let target = settings
                .as_object_mut()
                .ok_or(PrepareLiveSnapshotError::ExistingSettingsNotObject)?;
            for (key, value) in config {
                target.insert(key.clone(), value.clone());
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(PrepareLiveSnapshotError::ConfigNotObject),
    }
    set_selected_auth_type(&mut settings, auth_mode.selected_type())?;

    Ok(PreparedLiveSnapshot { env, settings })
}

fn project_env(env: Option<&Value>) -> Result<BTreeMap<String, String>, PrepareLiveSnapshotError> {
    let Some(env) = env else {
        return Ok(BTreeMap::new());
    };
    let env = env
        .as_object()
        .ok_or(PrepareLiveSnapshotError::EnvNotObject)?;
    env.iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or(PrepareLiveSnapshotError::EnvValueNotString)
        })
        .collect()
}

fn set_selected_auth_type(
    settings: &mut Value,
    selected_type: &str,
) -> Result<(), PrepareLiveSnapshotError> {
    let settings = settings
        .as_object_mut()
        .ok_or(PrepareLiveSnapshotError::ExistingSettingsNotObject)?;
    let security = object_field(settings, "security")?;
    let auth = object_field(security, "auth")?;
    auth.insert(
        "selectedType".to_owned(),
        Value::String(selected_type.to_owned()),
    );
    Ok(())
}

fn object_field<'a>(
    parent: &'a mut Map<String, Value>,
    key: &'static str,
) -> Result<&'a mut Map<String, Value>, PrepareLiveSnapshotError> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or(PrepareLiveSnapshotError::SettingsFieldNotObject(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_env_overlays_config_and_selects_api_key_auth() {
        let provider = json!({
            "env": {
                "GEMINI_API_KEY": "secret",
                "GEMINI_MODEL": "gemini-2.5-pro"
            },
            "config": {
                "theme": "light",
                "security": {"auth": {"ignored": true}}
            }
        });
        let existing = json!({
            "theme": "dark",
            "mcpServers": {"keep": true}
        });

        let snapshot = prepare_live_snapshot(&provider, Some(&existing), AuthMode::ApiKey)
            .expect("valid Gemini settings");

        assert_eq!(snapshot.env["GEMINI_API_KEY"], "secret");
        assert_eq!(snapshot.settings["theme"], "light");
        assert_eq!(snapshot.settings["mcpServers"]["keep"], true);
        assert_eq!(
            snapshot.settings["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
    }

    #[test]
    fn oauth_mode_accepts_an_empty_env_and_preserves_existing_settings() {
        let existing = json!({"mcpServers": {"keep": true}});

        let snapshot = prepare_live_snapshot(
            &json!({"env": {}, "config": null}),
            Some(&existing),
            AuthMode::OAuthPersonal,
        )
        .expect("valid OAuth settings");

        assert!(snapshot.env.is_empty());
        assert_eq!(snapshot.settings["mcpServers"]["keep"], true);
        assert_eq!(
            snapshot.settings["security"]["auth"]["selectedType"],
            "oauth-personal"
        );
    }

    #[test]
    fn rejects_invalid_or_incomplete_api_key_settings() {
        assert_eq!(
            prepare_live_snapshot(&json!([]), None, AuthMode::ApiKey),
            Err(PrepareLiveSnapshotError::SettingsNotObject)
        );
        assert_eq!(
            prepare_live_snapshot(
                &json!({"env": {"GEMINI_API_KEY": 1}}),
                None,
                AuthMode::ApiKey,
            ),
            Err(PrepareLiveSnapshotError::EnvValueNotString)
        );
        assert_eq!(
            prepare_live_snapshot(&json!({"env": {}}), None, AuthMode::ApiKey),
            Err(PrepareLiveSnapshotError::MissingApiKey)
        );
    }

    #[test]
    fn debug_output_redacts_both_live_documents() {
        let snapshot = prepare_live_snapshot(
            &json!({"env": {"GEMINI_API_KEY": "do-not-log"}}),
            None,
            AuthMode::ApiKey,
        )
        .expect("valid settings");

        let debug = format!("{snapshot:?}");
        assert_eq!(debug.matches("<redacted>").count(), 2);
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("GEMINI_API_KEY"));
    }
}
