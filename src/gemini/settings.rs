use std::fmt;

use serde_json::{Map, Value};

use super::{set_selected_auth_type, AuthMode, PrepareLiveSnapshotError};

/// Provider-owned top-level Gemini settings, ready to overlay on a native object.
/// Nested objects replace existing objects as a whole; this is not a deep merge.
#[derive(Clone, PartialEq)]
pub struct SettingsOverlay {
    settings: Map<String, Value>,
}

impl fmt::Debug for SettingsOverlay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingsOverlay")
            .field("settings", &"<redacted>")
            .finish()
    }
}

impl SettingsOverlay {
    /// Copy the provider's `config` object. Absent/null config owns no fields.
    /// Rejects other shapes without inspecting credentials or existing settings.
    pub fn from_config(config: Option<&Value>) -> Result<Self, PrepareLiveSnapshotError> {
        let settings = match config {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(settings)) => settings.clone(),
            Some(_) => return Err(PrepareLiveSnapshotError::ConfigNotObject),
        };
        Ok(Self { settings })
    }

    /// Select authentication in the incoming overlay. This claims the entire
    /// top-level `security` field when applied, including when config was absent.
    /// Rejects non-object `security` or `security.auth`; does not validate login.
    pub fn with_auth_mode(mut self, mode: AuthMode) -> Result<Self, PrepareLiveSnapshotError> {
        set_selected_auth_type(&mut self.settings, mode.selected_type())?;
        Ok(self)
    }

    /// Replace matching top-level keys, preserving every other existing field.
    /// Hosts decide how to handle missing or non-object native documents before
    /// supplying the object. No filesystem or credential validation is performed.
    ///
    /// ```
    /// use cc_switch_core::gemini::{AuthMode, SettingsOverlay};
    /// use serde_json::{json, Map, Value};
    /// let incoming = json!({"theme": "dark"});
    /// let overlay = SettingsOverlay::from_config(Some(&incoming))?
    ///     .with_auth_mode(AuthMode::OAuthPersonal)?;
    /// let result = Value::Object(overlay.apply_to(Map::new()));
    /// assert_eq!(result["theme"], "dark");
    /// assert_eq!(result["security"]["auth"]["selectedType"], "oauth-personal");
    /// # Ok::<(), cc_switch_core::gemini::PrepareLiveSnapshotError>(())
    /// ```
    pub fn apply_to(self, mut existing: Map<String, Value>) -> Map<String, Value> {
        existing.extend(self.settings);
        existing
    }
}
