//! Pi provider-entry projection.

use serde_json::Value;
use thiserror::Error;

use crate::ProviderEntry;

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
