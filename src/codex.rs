//! Codex live-configuration preparation.

use std::{error::Error, fmt};

use serde_json::Value;

/// Owned values required by the Codex live-write pipeline.
#[derive(Clone, PartialEq)]
pub struct PreparedLiveSnapshot {
    pub auth: Value,
    pub config: Option<String>,
}

impl fmt::Debug for PreparedLiveSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLiveSnapshot")
            .field("auth", &"<redacted>")
            .field("config", &"<redacted>")
            .finish()
    }
}

/// Validation errors returned while preparing a Codex live snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareLiveSnapshotError {
    SettingsNotObject,
    MissingAuth,
}

impl fmt::Display for PrepareLiveSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SettingsNotObject => "Codex settings must be a JSON object",
            Self::MissingAuth => "Codex settings are missing the 'auth' field",
        })
    }
}

impl Error for PrepareLiveSnapshotError {}

/// Extracts the values consumed by the Codex live-write pipeline.
pub fn prepare_live_snapshot(
    settings: &Value,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    let object = settings
        .as_object()
        .ok_or(PrepareLiveSnapshotError::SettingsNotObject)?;
    let auth = object
        .get("auth")
        .ok_or(PrepareLiveSnapshotError::MissingAuth)?;
    Ok(PreparedLiveSnapshot {
        auth: auth.clone(),
        config: object
            .get("config")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Errors returned by the strict Codex projection used by new live writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareStrictLiveSnapshotError {
    SettingsNotObject,
    MissingAuth,
    AuthNotObject,
    ConfigNotString,
    InvalidConfig,
}

impl fmt::Display for PrepareStrictLiveSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SettingsNotObject => "Codex settings must be a JSON object",
            Self::MissingAuth => "Codex settings are missing the 'auth' field",
            Self::AuthNotObject => "Codex 'auth' must be a JSON object",
            Self::ConfigNotString => "Codex 'config' must be a TOML string",
            Self::InvalidConfig => "Codex 'config' must be valid TOML",
        })
    }
}

impl Error for PrepareStrictLiveSnapshotError {}

/// Validates the native Codex shapes before preparing a live snapshot.
pub fn prepare_strict_live_snapshot(
    settings: &Value,
) -> Result<PreparedLiveSnapshot, PrepareStrictLiveSnapshotError> {
    let object = settings
        .as_object()
        .ok_or(PrepareStrictLiveSnapshotError::SettingsNotObject)?;
    let auth = object
        .get("auth")
        .ok_or(PrepareStrictLiveSnapshotError::MissingAuth)?;
    if !auth.is_object() {
        return Err(PrepareStrictLiveSnapshotError::AuthNotObject);
    }
    let config = match object.get("config") {
        Some(Value::String(config)) => {
            config
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| PrepareStrictLiveSnapshotError::InvalidConfig)?;
            Some(config.clone())
        }
        Some(Value::Null) | None => None,
        Some(_) => return Err(PrepareStrictLiveSnapshotError::ConfigNotString),
    };

    Ok(PreparedLiveSnapshot {
        auth: auth.clone(),
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_owned_live_values_without_changing_the_input() {
        let settings = json!({
            "auth": {"OPENAI_API_KEY": "secret"},
            "config": "model = \"gpt-5\"",
            "metadata": {"preserved": true}
        });

        let snapshot = prepare_live_snapshot(&settings).expect("valid settings");

        assert_eq!(snapshot.auth, json!({"OPENAI_API_KEY": "secret"}));
        assert_eq!(snapshot.config.as_deref(), Some("model = \"gpt-5\""));
        assert_eq!(settings["metadata"], json!({"preserved": true}));
    }

    #[test]
    fn treats_absent_or_non_string_config_as_absent() {
        for settings in [json!({"auth": null}), json!({"auth": [], "config": 1})] {
            let snapshot = prepare_live_snapshot(&settings).expect("valid settings");

            assert_eq!(snapshot.config, None);
        }
    }

    #[test]
    fn rejects_non_object_settings() {
        assert_eq!(
            prepare_live_snapshot(&json!([])),
            Err(PrepareLiveSnapshotError::SettingsNotObject)
        );
    }

    #[test]
    fn rejects_settings_without_auth() {
        assert_eq!(
            prepare_live_snapshot(&json!({"config": "model = \"gpt-5\""})),
            Err(PrepareLiveSnapshotError::MissingAuth)
        );
    }

    #[test]
    fn strict_snapshot_rejects_invalid_auth_and_config_shapes() {
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": null})),
            Err(PrepareStrictLiveSnapshotError::AuthNotObject)
        );
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": {}, "config": 1})),
            Err(PrepareStrictLiveSnapshotError::ConfigNotString)
        );
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": {}, "config": "not = [toml"})),
            Err(PrepareStrictLiveSnapshotError::InvalidConfig)
        );
    }

    #[test]
    fn strict_snapshot_accepts_valid_native_values() {
        let snapshot = prepare_strict_live_snapshot(&json!({
            "auth": {"OPENAI_API_KEY": "secret"},
            "config": "model = \"gpt-5\""
        }))
        .expect("valid strict snapshot");

        assert_eq!(snapshot.config.as_deref(), Some("model = \"gpt-5\""));
    }

    #[test]
    fn debug_output_redacts_live_values() {
        let snapshot = prepare_live_snapshot(&json!({
            "auth": {"OPENAI_API_KEY": "do-not-log"},
            "config": "experimental_bearer_token = \"also-private\""
        }))
        .expect("valid settings");

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("<redacted>"));
        for private in [
            "OPENAI_API_KEY",
            "do-not-log",
            "experimental_bearer_token",
            "also-private",
        ] {
            assert!(!debug.contains(private));
        }
    }
}
