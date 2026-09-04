//! Single built-in application integration catalog.

use crate::{
    mcp::{CLAUDE_MCP, CODEX_MCP, GEMINI_MCP, GROKBUILD_MCP, HERMES_MCP, OPENCODE_MCP},
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativePolicyBehavior, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{
        self, SimpleProviderBehavior, CLAUDE_DESKTOP_FORM, CLAUDE_FORM, CODEX_FORM, GEMINI_FORM,
        GROKBUILD_FORM, HERMES_FORM, OPENCLAW_FORM, OPENCODE_FORM,
    },
    AppType, LogicalTarget, NativeResourcePath, SimpleProviderFormDescriptor, SkillAppContract,
    SkillConfigTarget, SkillDiscovery,
};

/// All contracts that must be registered for one built-in application.
#[derive(Debug)]
pub(crate) struct AppIntegration {
    descriptor: AppDescriptor,
    targets: &'static [LogicalTarget],
    simple_provider_form: &'static SimpleProviderFormDescriptor,
    simple_provider_behavior: SimpleProviderBehavior,
    native_import_behavior: NativeImportBehavior,
    native_projection_behavior: NativeProjectionBehavior,
}

impl AppIntegration {
    pub(crate) const fn new(
        descriptor: AppDescriptor,
        targets: &'static [LogicalTarget],
        simple_provider_form: &'static SimpleProviderFormDescriptor,
        simple_provider_behavior: SimpleProviderBehavior,
        native_import_behavior: NativeImportBehavior,
        native_projection_behavior: NativeProjectionBehavior,
    ) -> Self {
        Self {
            descriptor,
            targets,
            simple_provider_form,
            simple_provider_behavior,
            native_import_behavior,
            native_projection_behavior,
        }
    }

    pub(crate) const fn descriptor(&self) -> &AppDescriptor {
        &self.descriptor
    }

    pub(crate) const fn targets(&self) -> &'static [LogicalTarget] {
        self.targets
    }

    pub(crate) const fn simple_provider_form(&self) -> &SimpleProviderFormDescriptor {
        self.simple_provider_form
    }

    pub(crate) const fn simple_provider_behavior(&self) -> SimpleProviderBehavior {
        self.simple_provider_behavior
    }

    pub(crate) const fn native_import_behavior(&self) -> NativeImportBehavior {
        self.native_import_behavior
    }

    pub(crate) const fn native_projection_behavior(&self) -> NativeProjectionBehavior {
        self.native_projection_behavior
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
            NativeResourcePath::relative("skills"),
        )),
        &[LogicalTarget::ClaudeSettings],
        &CLAUDE_FORM,
        SimpleProviderBehavior::new(
            simple_provider::extract_claude_like,
            simple_provider::project_claude,
            false,
        ),
        NativeImportBehavior::new(native_import::import_claude),
        NativeProjectionBehavior::new(
            projection::claude_plan,
            None,
            projection::declared_native_targets,
            NativeContextRequirement::Standard,
        ),
    ),
    AppIntegration::new(
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
        SimpleProviderBehavior::new(
            simple_provider::extract_claude_like,
            simple_provider::project_claude_desktop,
            false,
        ),
        NativeImportBehavior::new(native_import::import_claude_desktop),
        NativeProjectionBehavior::new(
            projection::claude_desktop_plan,
            None,
            projection::declared_native_targets,
            NativeContextRequirement::ClaudeDesktop,
        ),
    ),
    AppIntegration::new(
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
            NativeResourcePath::relative("skills"),
        )),
        &[
            LogicalTarget::CodexAuth,
            LogicalTarget::CodexConfig,
            LogicalTarget::CodexModelCatalog,
        ],
        &CODEX_FORM,
        SimpleProviderBehavior::new(
            simple_provider::extract_codex,
            simple_provider::project_codex,
            false,
        ),
        NativeImportBehavior::new(native_import::import_codex),
        NativeProjectionBehavior::new(
            projection::codex_plan,
            None,
            projection::codex_native_targets,
            NativeContextRequirement::Standard,
        )
        .with_policy(NativePolicyBehavior::new(
            projection::codex_policy_plan,
            projection::codex_policy_targets,
        )),
    ),
    AppIntegration::new(
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
            NativeResourcePath::relative("skills"),
        )),
        &[LogicalTarget::GeminiEnv, LogicalTarget::GeminiSettings],
        &GEMINI_FORM,
        SimpleProviderBehavior::new(
            simple_provider::extract_gemini,
            simple_provider::project_gemini,
            false,
        ),
        NativeImportBehavior::new(native_import::import_gemini),
        NativeProjectionBehavior::new(
            projection::gemini_plan,
            None,
            projection::declared_native_targets,
            NativeContextRequirement::Standard,
        ),
    ),
    AppIntegration::new(
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
            NativeResourcePath::relative("skills"),
        )),
        &[LogicalTarget::GrokConfig],
        &GROKBUILD_FORM,
        SimpleProviderBehavior::new(
            simple_provider::extract_grokbuild,
            simple_provider::project_grokbuild,
            true,
        ),
        NativeImportBehavior::new(native_import::import_grokbuild),
        NativeProjectionBehavior::new(
            projection::grokbuild_plan,
            None,
            projection::declared_native_targets,
            NativeContextRequirement::Standard,
        ),
    ),
    AppIntegration::new(
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
            NativeResourcePath::relative("skills"),
        )),
        &[LogicalTarget::OpenCodeConfig],
        &OPENCODE_FORM,
        SimpleProviderBehavior::new(
            simple_provider::extract_opencode,
            simple_provider::project_opencode,
            false,
        ),
        NativeImportBehavior::new(native_import::import_opencode),
        NativeProjectionBehavior::new(
            projection::opencode_plan,
            Some(projection::remove_opencode),
            projection::declared_native_targets,
            NativeContextRequirement::Standard,
        ),
    ),
    AppIntegration::new(
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
        SimpleProviderBehavior::new(
            simple_provider::extract_openai_array_provider,
            simple_provider::project_openclaw,
            false,
        ),
        NativeImportBehavior::new(native_import::import_openclaw),
        NativeProjectionBehavior::new(
            projection::openclaw_plan,
            Some(projection::remove_openclaw),
            projection::declared_native_targets,
            NativeContextRequirement::Standard,
        ),
    ),
    AppIntegration::new(
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
    ),
    crate::pi::INTEGRATION,
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
