//! Grok Build live-configuration projection.

use std::fmt;

use serde_json::Value;
use thiserror::Error;
use toml_edit::{DocumentMut, TableLike};

use crate::{
    integration::AppIntegration,
    mcp::GROKBUILD_MCP,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, GROKBUILD_FORM},
    AppType, LogicalTarget, NativeConfigRoot, NativeResourcePath, SkillAppContract,
    SkillConfigTarget, SkillDiscovery,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::LocalProxy,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::GrokBuild,
        "grokbuild",
        "Grok Build",
        "grok",
        ProviderConfigurationMode::Switch,
        CAPABILITIES,
        &["grok-build", "grok_build", "grok"],
    )
    .with_config_root(NativeConfigRoot::home_relative(".grok"))
    .with_mcp(&GROKBUILD_MCP)
    .with_skills(SkillAppContract::catalog(
        "enabled_grokbuild",
        SkillDiscovery::NativeAndUnified,
        Some(SkillConfigTarget::GrokConfig),
        NativeResourcePath::relative("skills"),
    )),
    &[LogicalTarget::GrokConfig],
    &GROKBUILD_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_grokbuild,
        simple_provider::project_grokbuild,
        true,
    ),
    NativeImportBehavior::new(native_import::import_grokbuild),
    NativeProjectionBehavior::new(
        projection::grokbuild_plan,
        None,
        projection::declared_native_targets,
        NativeContextRequirement::Standard,
    ),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    Official,
    Custom,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedLiveSnapshot {
    pub config: String,
}

impl fmt::Debug for PreparedLiveSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLiveSnapshot")
            .field("config", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareLiveSnapshotError {
    #[error("Grok Build settings must be a JSON object")]
    SettingsNotObject,
    #[error("Grok Build settings are missing the string 'config' field")]
    MissingConfig,
    #[error("Grok Build config must be valid TOML")]
    InvalidToml,
    #[error("Grok Build custom config is missing required model fields")]
    InvalidCustomConfig,
}

/// Extracts and validates the provider-owned Grok Build TOML document.
///
/// Official mode accepts an empty document or unrelated tables because Grok
/// supplies its own models and OAuth flow. Custom mode requires the selected
/// model table and either `api_key` or `env_key` credentials.
pub fn prepare_live_snapshot(
    settings: &Value,
    mode: ProviderMode,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    let settings = settings
        .as_object()
        .ok_or(PrepareLiveSnapshotError::SettingsNotObject)?;
    let config = settings
        .get("config")
        .and_then(Value::as_str)
        .ok_or(PrepareLiveSnapshotError::MissingConfig)?;
    let document = config
        .parse::<DocumentMut>()
        .map_err(|_| PrepareLiveSnapshotError::InvalidToml)?;
    if mode == ProviderMode::Custom {
        validate_custom_document(&document)?;
    }
    Ok(PreparedLiveSnapshot {
        config: config.to_owned(),
    })
}

fn validate_custom_document(document: &DocumentMut) -> Result<(), PrepareLiveSnapshotError> {
    let default_model = document
        .get("models")
        .and_then(|models| models.get("default"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(PrepareLiveSnapshotError::InvalidCustomConfig)?;
    let selected = document
        .get("model")
        .and_then(|models| models.get(default_model))
        .and_then(|model| model.as_table_like())
        .ok_or(PrepareLiveSnapshotError::InvalidCustomConfig)?;

    for field in ["model", "base_url", "name", "api_backend"] {
        required_string(selected, field)?;
    }
    if optional_string(selected, "api_key").is_none()
        && optional_string(selected, "env_key").is_none()
    {
        return Err(PrepareLiveSnapshotError::InvalidCustomConfig);
    }
    selected
        .get("context_window")
        .and_then(|value| value.as_integer())
        .filter(|value| *value > 0)
        .ok_or(PrepareLiveSnapshotError::InvalidCustomConfig)?;
    Ok(())
}

fn required_string(table: &dyn TableLike, key: &str) -> Result<(), PrepareLiveSnapshotError> {
    optional_string(table, key)
        .map(drop)
        .ok_or(PrepareLiveSnapshotError::InvalidCustomConfig)
}

fn optional_string<'a>(table: &'a dyn TableLike, key: &str) -> Option<&'a str> {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CUSTOM_CONFIG: &str = r#"[models]
default = "grok-custom"

[model.grok-custom]
model = "grok-4.5"
base_url = "https://example.com/v1"
name = "Example"
api_key = "secret"
api_backend = "responses"
context_window = 500000
"#;

    #[test]
    fn prepares_complete_custom_config_without_normalizing_it() {
        let settings = json!({"config": CUSTOM_CONFIG, "future": {"keep": true}});

        let snapshot =
            prepare_live_snapshot(&settings, ProviderMode::Custom).expect("valid custom settings");

        assert_eq!(snapshot.config, CUSTOM_CONFIG);
        assert_eq!(settings["future"]["keep"], true);
    }

    #[test]
    fn official_mode_accepts_empty_and_unrelated_documents() {
        for config in ["", "[mcp_servers.echo]\ncommand = \"echo\"\n"] {
            let snapshot =
                prepare_live_snapshot(&json!({"config": config}), ProviderMode::Official)
                    .expect("valid official settings");

            assert_eq!(snapshot.config, config);
        }
    }

    #[test]
    fn custom_mode_rejects_invalid_or_incomplete_documents() {
        assert_eq!(
            prepare_live_snapshot(&json!({"config": "not = [toml"}), ProviderMode::Custom),
            Err(PrepareLiveSnapshotError::InvalidToml)
        );
        assert_eq!(
            prepare_live_snapshot(
                &json!({"config": "[models]\ndefault = \"missing\""}),
                ProviderMode::Custom
            ),
            Err(PrepareLiveSnapshotError::InvalidCustomConfig)
        );
    }

    #[test]
    fn debug_output_redacts_config() {
        let snapshot =
            prepare_live_snapshot(&json!({"config": CUSTOM_CONFIG}), ProviderMode::Custom)
                .expect("valid settings");

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("api_key"));
    }
}
