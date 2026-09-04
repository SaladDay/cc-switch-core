//! OpenCode provider-entry projection.

use serde_json::Value;
use thiserror::Error;

use crate::{
    integration::AppIntegration,
    mcp::OPENCODE_MCP,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, OPENCODE_FORM},
    AppType, LogicalTarget, NativeConfigRoot, NativeResourcePath, ProviderEntry, SkillAppContract,
    SkillDiscovery,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::OpenCode,
        "opencode",
        "OpenCode",
        "opencode",
        ProviderConfigurationMode::Additive,
        CAPABILITIES,
        &[],
    )
    .with_config_root(NativeConfigRoot::home_relative(".config/opencode"))
    .with_mcp(&OPENCODE_MCP)
    .with_skills(SkillAppContract::catalog(
        "enabled_opencode",
        SkillDiscovery::NativeAndUnified,
        None,
        NativeResourcePath::relative("skills"),
    )),
    &[LogicalTarget::OpenCodeConfig],
    &OPENCODE_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_opencode,
        simple_provider::project_opencode,
        false,
    ),
    NativeImportBehavior::new(native_import::import_opencode),
    NativeProjectionBehavior::new(
        projection::opencode_plan,
        Some(projection::remove_opencode),
        projection::declared_native_targets,
        NativeContextRequirement::Standard,
    ),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareProviderEntryError {
    #[error("OpenCode provider key cannot be empty")]
    EmptyProviderKey,
    #[error("OpenCode settings must be a JSON object")]
    SettingsNotObject,
    #[error("OpenCode full config does not contain the requested provider fragment")]
    MissingProviderFragment,
    #[error("OpenCode provider config must contain 'npm' or 'options'")]
    InvalidProviderFragment,
}

/// Validates one provider fragment for `opencode.json.provider`.
pub fn prepare_provider_entry(
    provider_id: &str,
    settings: &Value,
) -> Result<ProviderEntry, PrepareProviderEntryError> {
    if provider_id.trim().is_empty() {
        return Err(PrepareProviderEntryError::EmptyProviderKey);
    }
    let config = settings
        .as_object()
        .ok_or(PrepareProviderEntryError::SettingsNotObject)?;
    let has_entry_point = config.contains_key("npm") || config.contains_key("options");
    let known_fields_are_valid = config.get("npm").is_none_or(Value::is_string)
        && config.get("options").is_none_or(Value::is_object)
        && config.get("models").is_none_or(Value::is_object);
    if !has_entry_point || !known_fields_are_valid {
        return Err(PrepareProviderEntryError::InvalidProviderFragment);
    }
    Ok(ProviderEntry::new(provider_id, settings.clone()))
}

/// Extracts and validates one provider from a complete `opencode.json` value.
pub fn prepare_provider_entry_from_full_config(
    provider_id: &str,
    full_config: &Value,
) -> Result<ProviderEntry, PrepareProviderEntryError> {
    if provider_id.trim().is_empty() {
        return Err(PrepareProviderEntryError::EmptyProviderKey);
    }
    let fragment = full_config
        .as_object()
        .ok_or(PrepareProviderEntryError::SettingsNotObject)?
        .get("provider")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(provider_id))
        .ok_or(PrepareProviderEntryError::MissingProviderFragment)?;
    prepare_provider_entry(provider_id, fragment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_native_fragments_without_guessing_their_layer() {
        let fragment = json!({
            "$schema": "future-extension",
            "npm": "@ai-sdk/openai-compatible",
            "future": true
        });
        assert_eq!(
            prepare_provider_entry("example", &fragment)
                .expect("valid fragment")
                .config,
            fragment
        );

        let full = json!({
            "$schema": "https://opencode.ai/config.json",
            "provider": {"example": {"options": {"apiKey": "secret"}}},
            "theme": "dark"
        });
        assert_eq!(
            prepare_provider_entry_from_full_config("example", &full)
                .expect("valid full config")
                .config,
            json!({"options": {"apiKey": "secret"}})
        );
    }

    #[test]
    fn rejects_missing_or_unrecognizable_fragments() {
        assert_eq!(
            prepare_provider_entry(" ", &json!({"options": {}})),
            Err(PrepareProviderEntryError::EmptyProviderKey)
        );
        assert_eq!(
            prepare_provider_entry_from_full_config("missing", &json!({"provider": {}})),
            Err(PrepareProviderEntryError::MissingProviderFragment)
        );
        assert_eq!(
            prepare_provider_entry("example", &json!({"name": "Example"})),
            Err(PrepareProviderEntryError::InvalidProviderFragment)
        );
        for settings in [
            json!({"npm": 42}),
            json!({"options": []}),
            json!({"npm": "package", "models": []}),
        ] {
            assert_eq!(
                prepare_provider_entry("example", &settings),
                Err(PrepareProviderEntryError::InvalidProviderFragment)
            );
        }
    }
}
