//! Claude Code live-configuration projection.

use std::{error::Error, fmt};

use serde_json::Value;

/// A validated Claude Code settings document ready for the live-write layer.
#[derive(Clone, PartialEq)]
pub struct PreparedLiveSnapshot {
    pub settings: Value,
}

impl fmt::Debug for PreparedLiveSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLiveSnapshot")
            .field("settings", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareLiveSnapshotError {
    SettingsNotObject,
}

impl fmt::Display for PrepareLiveSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Claude settings must be a JSON object")
    }
}

impl Error for PrepareLiveSnapshotError {}

/// Removes CC Switch metadata before a provider configuration is written to
/// Claude Code's `settings.json`.
pub fn prepare_live_settings(settings: &Value) -> Value {
    let mut live = settings.clone();
    if let Some(object) = live.as_object_mut() {
        for key in [
            "api_format",
            "apiFormat",
            "openrouter_compat_mode",
            "openrouterCompatMode",
        ] {
            object.remove(key);
        }
    }
    live
}

/// Validates and prepares the canonical Claude Code live snapshot.
///
/// [`prepare_live_settings`] remains available for compatibility with early
/// consumers that accepted arbitrary JSON values. New live writers should use
/// this strict entry point.
pub fn prepare_live_snapshot(
    settings: &Value,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    if !settings.is_object() {
        return Err(PrepareLiveSnapshotError::SettingsNotObject);
    }
    Ok(PreparedLiveSnapshot {
        settings: prepare_live_settings(settings),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_only_top_level_cc_switch_metadata() {
        let settings = json!({
            "api_format": "anthropic",
            "apiFormat": "openai_responses",
            "openrouter_compat_mode": true,
            "openrouterCompatMode": false,
            "env": {"ANTHROPIC_AUTH_TOKEN": "secret"},
            "nested": {"apiFormat": "preserved"}
        });

        assert_eq!(
            prepare_live_settings(&settings),
            json!({
                "env": {"ANTHROPIC_AUTH_TOKEN": "secret"},
                "nested": {"apiFormat": "preserved"}
            })
        );
        assert!(settings.get("api_format").is_some());
    }

    #[test]
    fn leaves_non_object_settings_unchanged() {
        for settings in [Value::Null, json!(["value"]), json!("value")] {
            assert_eq!(prepare_live_settings(&settings), settings);
        }
    }

    #[test]
    fn strict_snapshot_rejects_non_object_settings() {
        assert_eq!(
            prepare_live_snapshot(&json!([])),
            Err(PrepareLiveSnapshotError::SettingsNotObject)
        );
    }

    #[test]
    fn strict_snapshot_redacts_debug_output() {
        let snapshot = prepare_live_snapshot(&json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "do-not-log"}
        }))
        .expect("valid settings");

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!debug.contains("do-not-log"));
    }
}
