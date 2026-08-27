//! Claude Code live-configuration projection.

use serde_json::Value;

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
}
