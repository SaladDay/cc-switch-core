use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::registry::{
    builtin_app_registry, descriptor_for, AppCapability, ProviderConfigurationMode,
};

/// An application whose provider configuration can be managed by CC Switch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    Claude,
    #[serde(
        rename = "claude-desktop",
        alias = "claude_desktop",
        alias = "claudeDesktop"
    )]
    ClaudeDesktop,
    Codex,
    Gemini,
    GrokBuild,
    OpenCode,
    OpenClaw,
    Hermes,
    Pi,
}

impl AppType {
    /// Returns the stable identifier used in storage and IPC payloads.
    pub fn as_str(&self) -> &str {
        descriptor_for(self).id()
    }

    /// Returns whether every provider coexists in the application's live file.
    pub fn is_additive_mode(&self) -> bool {
        descriptor_for(self).configuration_mode() == ProviderConfigurationMode::Additive
    }

    /// Returns whether the application can be routed through the local proxy.
    pub fn supports_local_proxy(&self) -> bool {
        descriptor_for(self).supports(AppCapability::LocalProxy)
    }

    /// Iterates over all built-in application types in display order.
    pub fn all() -> impl Iterator<Item = Self> {
        builtin_app_registry()
            .descriptors()
            .map(|descriptor| descriptor.app().clone())
    }
}

/// Returned when a string is not a supported application identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAppTypeError {
    app_id: String,
}

impl fmt::Display for ParseAppTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "不支持的应用标识: '{}'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi。 (Unsupported app id: '{}'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi.)",
            self.app_id, self.app_id
        )
    }
}

impl std::error::Error for ParseAppTypeError {}

impl FromStr for AppType {
    type Err = ParseAppTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_lowercase();
        builtin_app_registry()
            .find(&normalized)
            .map(|descriptor| descriptor.app().clone())
            .ok_or(ParseAppTypeError { app_id: normalized })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_apps_keep_their_stable_order_and_identifiers() {
        let apps: Vec<_> = AppType::all().collect();
        let ids: Vec<_> = apps.iter().map(AppType::as_str).collect();

        assert_eq!(
            ids,
            [
                "claude",
                "claude-desktop",
                "codex",
                "gemini",
                "grokbuild",
                "opencode",
                "openclaw",
                "hermes",
                "pi",
            ]
        );
    }

    #[test]
    fn parses_existing_aliases_case_insensitively() {
        let cases = [
            (" ClAuDe ", AppType::Claude),
            ("claude-desktop", AppType::ClaudeDesktop),
            ("claude_desktop", AppType::ClaudeDesktop),
            ("claudeDesktop", AppType::ClaudeDesktop),
            ("codex", AppType::Codex),
            ("gemini", AppType::Gemini),
            ("grokbuild", AppType::GrokBuild),
            ("grok-build", AppType::GrokBuild),
            ("grok_build", AppType::GrokBuild),
            ("grok", AppType::GrokBuild),
            ("opencode", AppType::OpenCode),
            ("openclaw", AppType::OpenClaw),
            ("hermes", AppType::Hermes),
            ("pi", AppType::Pi),
        ];

        for (input, expected) in cases {
            assert_eq!(input.parse::<AppType>(), Ok(expected));
        }
    }

    #[test]
    fn parse_error_preserves_the_existing_message() {
        let error = " Unknown ".parse::<AppType>().expect_err("invalid app id");

        assert_eq!(
            error.to_string(),
            "不支持的应用标识: 'unknown'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi。 (Unsupported app id: 'unknown'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi.)"
        );
    }

    #[test]
    fn serde_round_trips_stable_identifiers() {
        for app in AppType::all() {
            let json = serde_json::to_string(&app).expect("serialize app type");
            assert_eq!(json, format!("\"{}\"", app.as_str()));
            assert_eq!(
                serde_json::from_str::<AppType>(&json).expect("deserialize app type"),
                app
            );
        }

        assert_eq!(
            serde_json::from_str::<AppType>("\"claude_desktop\"")
                .expect("deserialize legacy alias"),
            AppType::ClaudeDesktop
        );
        assert_eq!(
            serde_json::from_str::<AppType>("\"claudeDesktop\"").expect("deserialize legacy alias"),
            AppType::ClaudeDesktop
        );
    }

    #[test]
    fn additive_mode_matches_existing_app_semantics() {
        for app in AppType::all() {
            let expected = matches!(
                app,
                AppType::OpenCode | AppType::OpenClaw | AppType::Hermes | AppType::Pi
            );
            assert_eq!(app.is_additive_mode(), expected);
        }
    }

    #[test]
    fn local_proxy_support_matches_existing_app_semantics() {
        for app in AppType::all() {
            let expected = matches!(
                app,
                AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild
            );
            assert_eq!(app.supports_local_proxy(), expected);
        }
    }
}
