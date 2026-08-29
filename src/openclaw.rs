//! OpenClaw provider-entry projection.

use serde_json::Value;
use thiserror::Error;

use crate::ProviderEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareProviderEntryError {
    #[error("OpenClaw provider key cannot be empty")]
    EmptyProviderKey,
    #[error("OpenClaw settings must be a JSON object")]
    SettingsNotObject,
    #[error("OpenClaw provider config has an invalid native field")]
    InvalidProviderFragment,
}

/// Validates one provider fragment for `openclaw.json.models.providers`.
pub fn prepare_provider_entry(
    provider_id: &str,
    settings: &Value,
) -> Result<ProviderEntry, PrepareProviderEntryError> {
    if provider_id.trim().is_empty() {
        return Err(PrepareProviderEntryError::EmptyProviderKey);
    }
    let settings = settings
        .as_object()
        .ok_or(PrepareProviderEntryError::SettingsNotObject)?;
    let optional_strings_are_valid = ["baseUrl", "apiKey", "api"]
        .iter()
        .all(|key| settings.get(*key).is_none_or(Value::is_string));
    let models_are_valid = settings.get("models").is_none_or(|models| {
        models.as_array().is_some_and(|models| {
            models.iter().all(|model| {
                model.as_object().is_some_and(|model| {
                    model.get("id").is_some_and(Value::is_string)
                        && ["name", "alias"]
                            .iter()
                            .all(|key| model.get(*key).is_none_or(Value::is_string))
                        && model.get("contextWindow").is_none_or(|value| {
                            value
                                .as_u64()
                                .is_some_and(|value| u32::try_from(value).is_ok())
                        })
                        && model.get("cost").is_none_or(valid_model_cost)
                })
            })
        })
    });
    let headers_are_valid = settings.get("headers").is_none_or(|headers| {
        headers
            .as_object()
            .is_some_and(|headers| headers.values().all(Value::is_string))
    });
    if !optional_strings_are_valid || !models_are_valid || !headers_are_valid {
        return Err(PrepareProviderEntryError::InvalidProviderFragment);
    }
    Ok(ProviderEntry::new(
        provider_id,
        Value::Object(settings.clone()),
    ))
}

fn valid_model_cost(cost: &Value) -> bool {
    cost.as_object().is_some_and(|cost| {
        ["input", "output"]
            .iter()
            .all(|key| cost.get(*key).and_then(Value::as_f64).is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_valid_provider_fragments() {
        let settings = json!({
            "baseUrl": "https://example.com",
            "apiKey": "secret",
            "models": [],
            "future": {"keep": true}
        });

        let entry = prepare_provider_entry("example", &settings).expect("valid settings");

        assert_eq!(entry.key, "example");
        assert_eq!(entry.config, settings);
    }

    #[test]
    fn rejects_non_provider_shapes() {
        assert_eq!(
            prepare_provider_entry("", &json!({"models": []})),
            Err(PrepareProviderEntryError::EmptyProviderKey)
        );
        assert_eq!(
            prepare_provider_entry("example", &json!([])),
            Err(PrepareProviderEntryError::SettingsNotObject)
        );
        assert!(prepare_provider_entry("example", &json!({})).is_ok());
        assert_eq!(
            prepare_provider_entry("example", &json!({"models": {}})),
            Err(PrepareProviderEntryError::InvalidProviderFragment)
        );
        for settings in [
            json!({"models": [null]}),
            json!({"models": [{"id": 42}]}),
            json!({"headers": {"Authorization": 1}}),
            json!({"models": [{"id": "model", "contextWindow": -1}]}),
            json!({"models": [{"id": "model", "cost": {"input": 1}}]}),
        ] {
            assert_eq!(
                prepare_provider_entry("example", &settings),
                Err(PrepareProviderEntryError::InvalidProviderFragment)
            );
        }
    }
}
