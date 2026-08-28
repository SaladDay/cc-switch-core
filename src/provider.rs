//! Source-neutral provider snapshots.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppType;

/// A lossless provider view shared across CC Switch applications.
///
/// Storage-specific fields and write operations deliberately remain in each
/// application. `settings` keeps the application's native configuration shape
/// so consumers can pass it to the matching live-configuration projector.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    pub id: String,
    pub app: AppType,
    pub name: String,
    pub settings: Value,
}

/// One validated provider fragment ready to be merged into an additive live
/// configuration by a consumer-owned file writer.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub key: String,
    pub config: Value,
}

impl ProviderEntry {
    pub fn new(key: impl Into<String>, config: Value) -> Self {
        Self {
            key: key.into(),
            config,
        }
    }
}

impl fmt::Debug for ProviderEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderEntry")
            .field("key", &self.key)
            .field("config", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for ProviderSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSnapshot")
            .field("id", &self.id)
            .field("app", &self.app)
            .field("name", &self.name)
            .field("settings", &"<redacted>")
            .finish()
    }
}

impl ProviderSnapshot {
    pub fn new(
        id: impl Into<String>,
        app: AppType,
        name: impl Into<String>,
        settings: Value,
    ) -> Self {
        Self {
            id: id.into(),
            app,
            name: name.into(),
            settings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_native_settings_without_normalizing_them() {
        let snapshot = ProviderSnapshot::new(
            "provider-1",
            AppType::Codex,
            "Example",
            json!({
                "auth": {"OPENAI_API_KEY": "secret"},
                "config": "model = \"gpt-5\"",
                "extensionOwned": {"preserved": true}
            }),
        );

        assert_eq!(snapshot.settings["extensionOwned"]["preserved"], true);
        assert_eq!(snapshot.app, AppType::Codex);
    }

    #[test]
    fn serde_uses_stable_cross_application_field_names() {
        let snapshot = ProviderSnapshot::new(
            "provider-1",
            AppType::Claude,
            "Example",
            json!({"env": {"ANTHROPIC_AUTH_TOKEN": "secret"}}),
        );

        let json = serde_json::to_value(&snapshot).expect("serialize provider snapshot");
        assert_eq!(json["app"], "claude");
        assert_eq!(json["settings"]["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        assert!(json.get("settingsConfig").is_none());
        assert_eq!(
            serde_json::from_value::<ProviderSnapshot>(json)
                .expect("deserialize provider snapshot"),
            snapshot
        );
    }

    #[test]
    fn debug_output_redacts_native_settings() {
        let snapshot = ProviderSnapshot::new(
            "provider-1",
            AppType::Claude,
            "Example",
            json!({"env": {"ANTHROPIC_AUTH_TOKEN": "do-not-log"}}),
        );

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("provider-1"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn provider_entry_debug_output_redacts_config() {
        let entry = ProviderEntry::new(
            "example",
            json!({"apiKey": "do-not-log", "baseUrl": "https://example.com"}),
        );

        let debug = format!("{entry:?}");
        assert!(debug.contains("example"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("apiKey"));
    }
}
