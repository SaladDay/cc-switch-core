//! Codex live-configuration preparation.

use std::{error::Error, fmt};

use serde_json::Value;

/// Owned values required by the Codex live-write pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLiveSnapshot {
    pub auth: Value,
    pub config: Option<String>,
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
}
