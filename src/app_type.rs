use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

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
}

impl AppType {
    /// Returns the stable identifier used in storage and IPC payloads.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::ClaudeDesktop => "claude-desktop",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::GrokBuild => "grokbuild",
            Self::OpenCode => "opencode",
            Self::OpenClaw => "openclaw",
            Self::Hermes => "hermes",
        }
    }

    /// Returns whether every provider coexists in the application's live file.
    pub fn is_additive_mode(&self) -> bool {
        matches!(self, Self::OpenCode | Self::OpenClaw | Self::Hermes)
    }

    /// Iterates over all built-in application types in display order.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::Claude,
            Self::ClaudeDesktop,
            Self::Codex,
            Self::Gemini,
            Self::GrokBuild,
            Self::OpenCode,
            Self::OpenClaw,
            Self::Hermes,
        ]
        .into_iter()
    }
}

/// Returned when a string is not a supported application identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAppTypeError {
    app_id: String,
}

impl ParseAppTypeError {
    /// Returns the normalized identifier that failed to parse.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

impl fmt::Display for ParseAppTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "不支持的应用标识: '{}'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes。 (Unsupported app id: '{}'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes.)",
            self.app_id, self.app_id
        )
    }
}

impl std::error::Error for ParseAppTypeError {}

impl FromStr for AppType {
    type Err = ParseAppTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_lowercase();
        match normalized.as_str() {
            "claude" => Ok(Self::Claude),
            "claude-desktop" | "claude_desktop" | "claudedesktop" => Ok(Self::ClaudeDesktop),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "grokbuild" | "grok-build" | "grok_build" | "grok" => Ok(Self::GrokBuild),
            "opencode" => Ok(Self::OpenCode),
            "openclaw" => Ok(Self::OpenClaw),
            "hermes" => Ok(Self::Hermes),
            _ => Err(ParseAppTypeError { app_id: normalized }),
        }
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
        ];

        for (input, expected) in cases {
            assert_eq!(input.parse::<AppType>(), Ok(expected));
        }
    }

    #[test]
    fn parse_error_preserves_the_existing_message() {
        let error = " Unknown ".parse::<AppType>().expect_err("invalid app id");

        assert_eq!(error.app_id(), "unknown");
        assert_eq!(
            error.to_string(),
            "不支持的应用标识: 'unknown'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes。 (Unsupported app id: 'unknown'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes.)"
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
            let expected = matches!(app, AppType::OpenCode | AppType::OpenClaw | AppType::Hermes);
            assert_eq!(app.is_additive_mode(), expected);
        }
    }
}
