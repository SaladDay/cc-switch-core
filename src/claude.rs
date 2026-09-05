//! Claude Code live-configuration projection.

use std::{error::Error, fmt};

use serde_json::Value;

use crate::{
    integration::AppIntegration,
    mcp::CLAUDE_MCP,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, CLAUDE_FORM},
    AppType, LogicalTarget, NativeConfigRoot, NativeResourcePath, SkillAppContract, SkillDiscovery,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::CommonConfiguration,
    AppCapability::LocalProxy,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::Claude,
        "claude",
        "Claude",
        "claude",
        ProviderConfigurationMode::Switch,
        CAPABILITIES,
        &[],
    )
    .with_config_root(NativeConfigRoot::home_relative(".claude"))
    .with_mcp(&CLAUDE_MCP)
    .with_skills(SkillAppContract::catalog(
        "enabled_claude",
        SkillDiscovery::NativeOnly,
        None,
        NativeResourcePath::relative("skills"),
    )),
    &[LogicalTarget::ClaudeSettings],
    &CLAUDE_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_claude_like,
        simple_provider::project_claude,
        false,
    ),
    NativeImportBehavior::new(native_import::import_claude),
    NativeProjectionBehavior::new(
        projection::claude_plan,
        None,
        projection::declared_native_targets,
        NativeContextRequirement::Standard,
    ),
);

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
    strip_internal_metadata(&mut live);
    live
}

/// Removes only the four top-level CC Switch metadata fields in place.
/// Non-object settings and nested fields are left unchanged. This does not
/// validate settings or normalize model keys.
pub fn strip_internal_metadata(settings: &mut Value) {
    if let Some(object) = settings.as_object_mut() {
        for key in [
            "api_format",
            "apiFormat",
            "openrouter_compat_mode",
            "openrouterCompatMode",
        ] {
            object.remove(key);
        }
    }
}

/// Fills absent Claude model-role keys and removes `ANTHROPIC_SMALL_FAST_MODEL`.
///
/// Haiku prefers the legacy small/fast model, then `ANTHROPIC_MODEL`; Sonnet and
/// Opus prefer the reverse. Only string sources are used, without trimming.
/// Existing role keys, even non-string values, and unrelated fields are retained.
/// A legacy key is removed regardless of its value. Returns whether anything
/// changed; settings without an object-valued `env` are left unchanged.
///
/// Hosts opt into this migration explicitly. Preparing a live snapshot does not
/// invoke it or choose when stored provider settings should be migrated.
///
/// ```
/// use cc_switch_core::claude::normalize_model_keys;
/// use serde_json::json;
///
/// let mut settings = json!({"env": {
///     "ANTHROPIC_MODEL": "main",
///     "ANTHROPIC_SMALL_FAST_MODEL": "fast"
/// }});
/// assert!(normalize_model_keys(&mut settings));
/// assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "fast");
/// assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "main");
/// assert!(!normalize_model_keys(&mut settings));
/// ```
pub fn normalize_model_keys(settings: &mut Value) -> bool {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return false;
    };
    let model = env
        .get("ANTHROPIC_MODEL")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let small_fast = env
        .get("ANTHROPIC_SMALL_FAST_MODEL")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut changed = false;
    for (key, fallback) in [
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            small_fast.as_ref().or(model.as_ref()),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            model.as_ref().or(small_fast.as_ref()),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            model.as_ref().or(small_fast.as_ref()),
        ),
    ] {
        if let Some(value) = fallback {
            if let serde_json::map::Entry::Vacant(entry) = env.entry(key) {
                entry.insert(Value::String(value.clone()));
                changed = true;
            }
        }
    }
    if env.remove("ANTHROPIC_SMALL_FAST_MODEL").is_some() {
        changed = true;
    }
    changed
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
