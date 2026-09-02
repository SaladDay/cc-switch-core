//! Single built-in application integration catalog.

use crate::{
    adapter::BuiltinAdapter,
    mcp::{CLAUDE_MCP, CODEX_MCP, GEMINI_MCP, GROKBUILD_MCP, HERMES_MCP, OPENCODE_MCP},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{
        CLAUDE_DESKTOP_FORM, CLAUDE_FORM, CODEX_FORM, GEMINI_FORM, GROKBUILD_FORM, HERMES_FORM,
        OPENCLAW_FORM, OPENCODE_FORM, PI_FORM,
    },
    AppType, LogicalTarget, SimpleProviderFormDescriptor, SkillAppContract, SkillConfigTarget,
    SkillDiscovery,
};

/// All contracts that must be registered for one built-in application.
pub(crate) struct AppIntegration {
    descriptor: AppDescriptor,
    adapter: BuiltinAdapter,
    simple_provider_form: &'static SimpleProviderFormDescriptor,
}

impl AppIntegration {
    const fn new(
        app: AppType,
        descriptor: AppDescriptor,
        targets: &'static [LogicalTarget],
        simple_provider_form: &'static SimpleProviderFormDescriptor,
    ) -> Self {
        Self {
            descriptor,
            adapter: BuiltinAdapter::new(app, targets),
            simple_provider_form,
        }
    }

    pub(crate) const fn descriptor(&self) -> &AppDescriptor {
        &self.descriptor
    }

    pub(crate) const fn adapter(&self) -> &BuiltinAdapter {
        &self.adapter
    }

    pub(crate) const fn simple_provider_form(&self) -> &SimpleProviderFormDescriptor {
        self.simple_provider_form
    }
}

const PROVIDER_LIVE_COMMON_PROXY_MCP_PROMPTS_SKILLS: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::CommonConfiguration,
    AppCapability::LocalProxy,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

const PROVIDER_LIVE_PROXY_MCP_PROMPTS_SKILLS: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::LocalProxy,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

const PROVIDER_LIVE_MCP_PROMPTS_SKILLS: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

const PROVIDER_LIVE_PROMPTS_SKILLS: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::Prompts,
    AppCapability::Skills,
];

const PROVIDER_LIVE_PROMPTS: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::Prompts,
];

const PROVIDER_LIVE: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
];

use SkillConfigTarget::{GeminiSettings, GrokConfig, HermesConfig};
use SkillDiscovery::{NativeAndUnified, NativeOnly};

static BUILTIN_APP_INTEGRATIONS: [AppIntegration; 9] = [
    AppIntegration::new(
        AppType::Claude,
        AppDescriptor::new(
            AppType::Claude,
            "claude",
            "Claude",
            "claude",
            ProviderConfigurationMode::Switch,
            PROVIDER_LIVE_COMMON_PROXY_MCP_PROMPTS_SKILLS,
            &[],
        )
        .with_mcp(&CLAUDE_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_claude",
            NativeOnly,
            None,
        )),
        &[LogicalTarget::ClaudeSettings],
        &CLAUDE_FORM,
    ),
    AppIntegration::new(
        AppType::ClaudeDesktop,
        AppDescriptor::new(
            AppType::ClaudeDesktop,
            "claude-desktop",
            "Claude Desktop",
            "claude",
            ProviderConfigurationMode::Switch,
            PROVIDER_LIVE,
            &["claude_desktop", "claudedesktop"],
        ),
        &[
            LogicalTarget::ClaudeDesktopNormalConfig,
            LogicalTarget::ClaudeDesktopThreepConfig,
            LogicalTarget::ClaudeDesktopProfile,
            LogicalTarget::ClaudeDesktopMeta,
        ],
        &CLAUDE_DESKTOP_FORM,
    ),
    AppIntegration::new(
        AppType::Codex,
        AppDescriptor::new(
            AppType::Codex,
            "codex",
            "Codex",
            "codex",
            ProviderConfigurationMode::Switch,
            PROVIDER_LIVE_COMMON_PROXY_MCP_PROMPTS_SKILLS,
            &[],
        )
        .with_mcp(&CODEX_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_codex",
            NativeAndUnified,
            None,
        )),
        &[
            LogicalTarget::CodexAuth,
            LogicalTarget::CodexConfig,
            LogicalTarget::CodexModelCatalog,
        ],
        &CODEX_FORM,
    ),
    AppIntegration::new(
        AppType::Gemini,
        AppDescriptor::new(
            AppType::Gemini,
            "gemini",
            "Gemini",
            "gemini",
            ProviderConfigurationMode::Switch,
            PROVIDER_LIVE_COMMON_PROXY_MCP_PROMPTS_SKILLS,
            &[],
        )
        .with_mcp(&GEMINI_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_gemini",
            NativeAndUnified,
            Some(GeminiSettings),
        )),
        &[LogicalTarget::GeminiEnv, LogicalTarget::GeminiSettings],
        &GEMINI_FORM,
    ),
    AppIntegration::new(
        AppType::GrokBuild,
        AppDescriptor::new(
            AppType::GrokBuild,
            "grokbuild",
            "Grok Build",
            "grok",
            ProviderConfigurationMode::Switch,
            PROVIDER_LIVE_PROXY_MCP_PROMPTS_SKILLS,
            &["grok-build", "grok_build", "grok"],
        )
        .with_mcp(&GROKBUILD_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_grokbuild",
            NativeAndUnified,
            Some(GrokConfig),
        )),
        &[LogicalTarget::GrokConfig],
        &GROKBUILD_FORM,
    ),
    AppIntegration::new(
        AppType::OpenCode,
        AppDescriptor::new(
            AppType::OpenCode,
            "opencode",
            "OpenCode",
            "opencode",
            ProviderConfigurationMode::Additive,
            PROVIDER_LIVE_MCP_PROMPTS_SKILLS,
            &[],
        )
        .with_mcp(&OPENCODE_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_opencode",
            NativeAndUnified,
            None,
        )),
        &[LogicalTarget::OpenCodeConfig],
        &OPENCODE_FORM,
    ),
    AppIntegration::new(
        AppType::OpenClaw,
        AppDescriptor::new(
            AppType::OpenClaw,
            "openclaw",
            "OpenClaw",
            "openclaw",
            ProviderConfigurationMode::Additive,
            PROVIDER_LIVE_PROMPTS,
            &[],
        ),
        &[LogicalTarget::OpenClawConfig],
        &OPENCLAW_FORM,
    ),
    AppIntegration::new(
        AppType::Hermes,
        AppDescriptor::new(
            AppType::Hermes,
            "hermes",
            "Hermes",
            "hermes",
            ProviderConfigurationMode::Additive,
            PROVIDER_LIVE_MCP_PROMPTS_SKILLS,
            &[],
        )
        .with_mcp(&HERMES_MCP)
        .with_skills(SkillAppContract::catalog(
            "enabled_hermes",
            NativeOnly,
            Some(HermesConfig),
        )),
        &[LogicalTarget::HermesConfig],
        &HERMES_FORM,
    ),
    AppIntegration::new(
        AppType::Pi,
        AppDescriptor::new(
            AppType::Pi,
            "pi",
            "Pi",
            "pi",
            ProviderConfigurationMode::Additive,
            PROVIDER_LIVE_PROMPTS_SKILLS,
            &[],
        )
        .with_skills(SkillAppContract::catalog(
            "enabled_pi",
            NativeAndUnified,
            None,
        )),
        &[LogicalTarget::PiModels],
        &PI_FORM,
    ),
];

pub(crate) fn builtin_app_integrations(
) -> impl ExactSizeIterator<Item = &'static AppIntegration> + DoubleEndedIterator + Clone {
    BUILTIN_APP_INTEGRATIONS.iter()
}

pub(crate) fn builtin_app_integration(app: &AppType) -> &'static AppIntegration {
    let index = match app {
        AppType::Claude => 0,
        AppType::ClaudeDesktop => 1,
        AppType::Codex => 2,
        AppType::Gemini => 3,
        AppType::GrokBuild => 4,
        AppType::OpenCode => 5,
        AppType::OpenClaw => 6,
        AppType::Hermes => 7,
        AppType::Pi => 8,
    };
    &BUILTIN_APP_INTEGRATIONS[index]
}
