//! Pi provider-entry projection.

use serde_json::Value;
use thiserror::Error;

use crate::{
    integration::AppIntegration,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, PI_FORM},
    AppType, LogicalTarget, NativeConfigRoot, NativeResourcePath, ProviderEntry, SkillAppContract,
    SkillDiscovery,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::Prompts,
    AppCapability::Skills,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::Pi,
        "pi",
        "Pi",
        "pi",
        ProviderConfigurationMode::Additive,
        CAPABILITIES,
        &[],
    )
    .with_model_fetch(&crate::model_fetch::BEARER_COMPATIBLE)
    .with_config_root(NativeConfigRoot::home_relative(".pi/agent"))
    .with_skills(SkillAppContract::catalog(
        "enabled_pi",
        SkillDiscovery::NativeAndUnified,
        None,
        NativeResourcePath::relative("skills"),
    )),
    &[LogicalTarget::PiModels],
    &PI_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_openai_array_provider,
        simple_provider::project_pi,
        false,
    ),
    NativeImportBehavior::new(native_import::import_pi),
    NativeProjectionBehavior::new(
        projection::pi_plan,
        Some(projection::remove_pi),
        projection::declared_native_targets,
        NativeContextRequirement::Standard,
    ),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareProviderEntryError {
    #[error("Pi provider key cannot be empty")]
    EmptyProviderKey,
    #[error("Pi provider configuration must be a JSON object")]
    SettingsNotObject,
}

/// Validates one `models.json.providers` entry without reading auth state.
pub fn prepare_provider_entry(
    provider_key: &str,
    settings: &Value,
) -> Result<ProviderEntry, PrepareProviderEntryError> {
    if provider_key.trim().is_empty() {
        return Err(PrepareProviderEntryError::EmptyProviderKey);
    }
    if !settings.is_object() {
        return Err(PrepareProviderEntryError::SettingsNotObject);
    }
    Ok(ProviderEntry::new(provider_key, settings.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_native_pi_provider_nodes() {
        let settings = json!({
            "baseUrl": "https://example.com",
            "apiKey": "secret",
            "models": [{"id": "model"}],
            "future": true
        });

        let entry = prepare_provider_entry("example", &settings).expect("valid settings");

        assert_eq!(entry.key, "example");
        assert_eq!(entry.config, settings);
    }

    #[test]
    fn rejects_empty_keys_and_non_object_settings() {
        assert_eq!(
            prepare_provider_entry(" ", &json!({})),
            Err(PrepareProviderEntryError::EmptyProviderKey)
        );
        assert_eq!(
            prepare_provider_entry("example", &json!([])),
            Err(PrepareProviderEntryError::SettingsNotObject)
        );
    }

    #[test]
    fn explicit_native_entries_define_ownership() {
        for (key, settings) in [
            ("anthropic", json!({})),
            ("native-oauth", json!({"oauth": "anthropic"})),
            ("empty-catalog", json!({"models": []})),
        ] {
            assert!(prepare_provider_entry(key, &settings).is_ok());
        }
    }
}
