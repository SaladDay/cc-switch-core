//! Hermes custom-provider projection.

use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    integration::AppIntegration,
    mcp::HERMES_MCP,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, HERMES_FORM},
    AppType, LogicalTarget, NativeResourcePath, ProviderEntry, SkillAppContract, SkillConfigTarget,
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
        AppType::Hermes,
        "hermes",
        "Hermes",
        "hermes",
        ProviderConfigurationMode::Additive,
        CAPABILITIES,
        &[],
    )
    .with_model_fetch(&crate::model_fetch::BEARER_COMPATIBLE)
    .with_mcp(&HERMES_MCP)
    .with_skills(SkillAppContract::catalog(
        "enabled_hermes",
        SkillDiscovery::NativeOnly,
        Some(SkillConfigTarget::HermesConfig),
        NativeResourcePath::relative("skills"),
    )),
    &[LogicalTarget::HermesConfig],
    &HERMES_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_hermes,
        simple_provider::project_hermes,
        false,
    ),
    NativeImportBehavior::new(native_import::import_hermes),
    NativeProjectionBehavior::new(
        projection::hermes_plan,
        Some(projection::hermes_remove_plan),
        projection::declared_native_targets,
        NativeContextRequirement::Standard,
    ),
);

const KEY_ALIASES: [(&str, &str); 5] = [
    ("baseUrl", "base_url"),
    ("apiKey", "api_key"),
    ("apiMode", "api_mode"),
    ("maxTokens", "max_tokens"),
    ("contextLength", "context_length"),
];
const FIELDS_TO_DROP: [&str; 3] = ["api", "_cc_source", "provider_key"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareProviderEntryError {
    #[error("Hermes provider key cannot be empty")]
    EmptyProviderKey,
    #[error("Hermes provider configuration must be a JSON object")]
    SettingsNotObject,
    #[error("Hermes 'models' must be an object or array")]
    InvalidModels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareModelDefaultsError {
    #[error("Hermes provider key cannot be empty")]
    EmptyProviderKey,
    #[error("Hermes model configuration must be a JSON object")]
    ModelNotObject,
}

/// Normalizes one provider into Hermes' writable `custom_providers` shape.
///
/// Historical camelCase aliases are healed, UI-only fields are removed, and
/// an array of model objects is converted into Hermes' model map.
pub fn prepare_provider_entry(
    provider_key: &str,
    settings: &Value,
) -> Result<ProviderEntry, PrepareProviderEntryError> {
    if provider_key.trim().is_empty() {
        return Err(PrepareProviderEntryError::EmptyProviderKey);
    }
    let mut config = settings
        .as_object()
        .cloned()
        .ok_or(PrepareProviderEntryError::SettingsNotObject)?;
    for (alias, native) in KEY_ALIASES {
        if let Some(value) = config.remove(alias) {
            config.entry(native.to_owned()).or_insert(value);
        }
    }
    for field in FIELDS_TO_DROP {
        config.remove(field);
    }

    if let Some(models) = config.get_mut("models") {
        match models {
            Value::Array(entries) => {
                *models = Value::Object(models_array_to_map(std::mem::take(entries)));
            }
            Value::Object(entries)
                if entries
                    .values()
                    .all(|entry| entry.is_object() || entry.is_null()) => {}
            Value::Object(_) => return Err(PrepareProviderEntryError::InvalidModels),
            _ => return Err(PrepareProviderEntryError::InvalidModels),
        }
    }
    let first_model = config
        .get("models")
        .and_then(Value::as_object)
        .and_then(|models| models.keys().next())
        .cloned();
    config.insert("name".to_owned(), Value::String(provider_key.to_owned()));
    match first_model {
        Some(model) => {
            config.insert("model".to_owned(), Value::String(model));
        }
        None => {
            config.remove("model");
        }
    }
    Ok(ProviderEntry::new(provider_key, Value::Object(config)))
}

/// Selects the provider and its first declared model while preserving every
/// other key in Hermes' top-level `model` section.
pub fn prepare_model_defaults(
    provider_key: &str,
    settings: &Value,
    current: Option<&Value>,
) -> Result<Value, PrepareModelDefaultsError> {
    if provider_key.trim().is_empty() {
        return Err(PrepareModelDefaultsError::EmptyProviderKey);
    }
    let mut model = match current {
        Some(value) => value
            .as_object()
            .cloned()
            .ok_or(PrepareModelDefaultsError::ModelNotObject)?,
        None => Map::new(),
    };
    model.insert(
        "provider".to_owned(),
        Value::String(provider_key.to_owned()),
    );
    if let Some(first_model) = first_model_id(settings) {
        model.insert("default".to_owned(), Value::String(first_model));
    }
    Ok(Value::Object(model))
}

fn first_model_id(settings: &Value) -> Option<String> {
    match settings.get("models")? {
        Value::Array(models) => models
            .first()
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        Value::Object(models) => models
            .keys()
            .map(String::as_str)
            .map(str::trim)
            .find(|id| !id.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn models_array_to_map(entries: Vec<Value>) -> Map<String, Value> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let mut entry = entry.as_object()?.clone();
            let id = entry.remove("id")?.as_str()?.trim().to_owned();
            (!id.is_empty()).then_some((id, Value::Object(entry)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_aliases_models_and_runtime_fields() {
        let settings = json!({
            "baseUrl": "https://example.com",
            "base_url": "https://native.example.com",
            "apiKey": "secret",
            "api": "openai-completions",
            "_cc_source": "custom_providers",
            "models": [
                {"id": "gpt-4o", "context_length": 128000},
                {"id": "", "ignored": true},
                "ignored"
            ],
            "future": {"keep": true}
        });

        let entry = prepare_provider_entry("example", &settings).expect("valid settings");

        assert_eq!(entry.config["name"], "example");
        assert_eq!(entry.config["model"], "gpt-4o");
        assert_eq!(entry.config["base_url"], "https://native.example.com");
        assert_eq!(entry.config["api_key"], "secret");
        assert_eq!(entry.config["models"]["gpt-4o"]["context_length"], 128000);
        assert_eq!(entry.config["future"]["keep"], true);
        assert!(entry.config.get("baseUrl").is_none());
        assert!(entry.config.get("api").is_none());
        assert!(entry.config.get("_cc_source").is_none());
    }

    #[test]
    fn rejects_invalid_keys_and_model_shapes() {
        assert_eq!(
            prepare_provider_entry("", &json!({})),
            Err(PrepareProviderEntryError::EmptyProviderKey)
        );
        assert_eq!(
            prepare_provider_entry("example", &json!({"models": "invalid"})),
            Err(PrepareProviderEntryError::InvalidModels)
        );
        assert_eq!(
            prepare_provider_entry("example", &json!({"models": {"model": "invalid"}})),
            Err(PrepareProviderEntryError::InvalidModels)
        );
    }

    #[test]
    fn model_defaults_switch_provider_and_preserve_other_fields() {
        let current = json!({
            "provider": "old",
            "default": "old-model",
            "context_length": 32000
        });
        let result = prepare_model_defaults(
            "new",
            &json!({"models": [{"id": "new-model"}]}),
            Some(&current),
        )
        .expect("valid defaults");

        assert_eq!(result["provider"], "new");
        assert_eq!(result["default"], "new-model");
        assert_eq!(result["context_length"], 32000);
    }

    #[test]
    fn model_defaults_keep_existing_default_without_models() {
        let result =
            prepare_model_defaults("new", &json!({}), Some(&json!({"default": "keep-me"})))
                .expect("valid defaults");

        assert_eq!(result["provider"], "new");
        assert_eq!(result["default"], "keep-me");
    }
}
