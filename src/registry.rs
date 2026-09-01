//! Built-in application metadata and capability registry.

use serde::Serialize;

use crate::{
    mcp::{
        McpAppContract, CLAUDE_MCP, CODEX_MCP, GEMINI_MCP, GROKBUILD_MCP, HERMES_MCP, OPENCODE_MCP,
    },
    AppType, SkillAppContract, SkillConfigTarget, SkillDiscovery,
};

/// A product-facing capability declared by an application.
///
/// Capabilities describe whether a product may expose a feature for an app.
/// They do not move the feature implementation into this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AppCapability {
    ProviderManagement,
    LiveConfiguration,
    CommonConfiguration,
    LocalProxy,
    Mcp,
    Prompts,
    Skills,
}

/// How provider configurations are activated in an application's native files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderConfigurationMode {
    /// One provider owns the active native configuration.
    Switch,
    /// Multiple providers coexist in the active native configuration.
    Additive,
}

/// Stable metadata for one built-in application.
///
/// Presentation layers may use `display_name` and `brand_key` as fallbacks,
/// while keeping localized labels, components, and styling product-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDescriptor {
    #[serde(skip_serializing)]
    app: AppType,
    id: &'static str,
    display_name: &'static str,
    brand_key: &'static str,
    configuration_mode: ProviderConfigurationMode,
    capabilities: &'static [AppCapability],
    #[serde(skip_serializing)]
    mcp_contract: Option<&'static McpAppContract>,
    #[serde(skip_serializing)]
    skill_contract: Option<SkillAppContract>,
    #[serde(skip_serializing)]
    aliases: &'static [&'static str],
}

impl AppDescriptor {
    const fn new(
        app: AppType,
        id: &'static str,
        display_name: &'static str,
        brand_key: &'static str,
        configuration_mode: ProviderConfigurationMode,
        capabilities: &'static [AppCapability],
        aliases: &'static [&'static str],
    ) -> Self {
        Self {
            app,
            id,
            display_name,
            brand_key,
            configuration_mode,
            capabilities,
            mcp_contract: None,
            skill_contract: None,
            aliases,
        }
    }

    const fn with_mcp(mut self, contract: &'static McpAppContract) -> Self {
        self.mcp_contract = Some(contract);
        self
    }

    const fn with_skills(mut self, contract: SkillAppContract) -> Self {
        self.skill_contract = Some(contract);
        self
    }

    /// Returns the built-in application type represented by this descriptor.
    pub fn app(&self) -> &AppType {
        &self.app
    }

    /// Returns the stable identifier used by storage and IPC payloads.
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// Returns the non-localized display-name fallback.
    pub fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// Returns the stable key used to select product-owned brand assets.
    pub fn brand_key(&self) -> &'static str {
        self.brand_key
    }

    /// Returns the provider activation mode.
    pub fn configuration_mode(&self) -> ProviderConfigurationMode {
        self.configuration_mode
    }

    /// Returns all declared capabilities in stable order.
    pub fn capabilities(&self) -> &'static [AppCapability] {
        self.capabilities
    }

    /// Returns whether the application declares a capability.
    pub fn supports(&self, capability: AppCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Returns the native MCP contract declared by this application.
    pub fn mcp_contract(&self) -> Option<&'static McpAppContract> {
        self.mcp_contract
    }

    /// Returns the application's installed-Skill behavior, when supported.
    pub const fn skill_contract(&self) -> Option<SkillAppContract> {
        self.skill_contract
    }

    fn matches_id(&self, normalized: &str) -> bool {
        self.id() == normalized || self.aliases.contains(&normalized)
    }
}

/// Read-only registry of the applications compiled into CC Switch products.
#[derive(Debug, Default)]
pub struct BuiltinAppRegistry {
    _private: (),
}

impl BuiltinAppRegistry {
    /// Returns every built-in descriptor in stable display order.
    pub fn descriptors(
        &self,
    ) -> impl ExactSizeIterator<Item = &'static AppDescriptor> + DoubleEndedIterator + Clone {
        BUILTIN_APP_DESCRIPTORS.iter().copied()
    }

    /// Resolves a stable identifier or supported legacy alias.
    pub fn find(&self, id: &str) -> Option<&'static AppDescriptor> {
        let normalized = id.trim().to_lowercase();
        self.descriptors()
            .find(|descriptor| descriptor.matches_id(&normalized))
    }

    /// Returns the descriptor for a built-in application type.
    pub fn for_app(&self, app: &AppType) -> &'static AppDescriptor {
        descriptor_for(app)
    }
}

/// Returns the process-wide built-in application registry.
pub fn builtin_app_registry() -> &'static BuiltinAppRegistry {
    &BUILTIN_APP_REGISTRY
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

static BUILTIN_APP_REGISTRY: BuiltinAppRegistry = BuiltinAppRegistry { _private: () };

use SkillConfigTarget::{GeminiSettings, GrokConfig, HermesConfig};
use SkillDiscovery::{NativeAndUnified, NativeOnly};

static CLAUDE_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static CLAUDE_DESKTOP_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
    AppType::ClaudeDesktop,
    "claude-desktop",
    "Claude Desktop",
    "claude",
    ProviderConfigurationMode::Switch,
    PROVIDER_LIVE,
    &["claude_desktop", "claudedesktop"],
);

static CODEX_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static GEMINI_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static GROKBUILD_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static OPENCODE_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static OPENCLAW_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
    AppType::OpenClaw,
    "openclaw",
    "OpenClaw",
    "openclaw",
    ProviderConfigurationMode::Additive,
    PROVIDER_LIVE_PROMPTS,
    &[],
);

static HERMES_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
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
));

static PI_DESCRIPTOR: AppDescriptor = AppDescriptor::new(
    AppType::Pi,
    "pi",
    "Pi",
    "pi",
    ProviderConfigurationMode::Additive,
    PROVIDER_LIVE_PROMPTS_SKILLS,
    &[],
)
.with_skills(SkillAppContract::native_directory(NativeAndUnified));

static BUILTIN_APP_DESCRIPTORS: [&AppDescriptor; 9] = [
    &CLAUDE_DESCRIPTOR,
    &CLAUDE_DESKTOP_DESCRIPTOR,
    &CODEX_DESCRIPTOR,
    &GEMINI_DESCRIPTOR,
    &GROKBUILD_DESCRIPTOR,
    &OPENCODE_DESCRIPTOR,
    &OPENCLAW_DESCRIPTOR,
    &HERMES_DESCRIPTOR,
    &PI_DESCRIPTOR,
];

pub(crate) fn descriptor_for(app: &AppType) -> &'static AppDescriptor {
    match app {
        AppType::Claude => &CLAUDE_DESCRIPTOR,
        AppType::ClaudeDesktop => &CLAUDE_DESKTOP_DESCRIPTOR,
        AppType::Codex => &CODEX_DESCRIPTOR,
        AppType::Gemini => &GEMINI_DESCRIPTOR,
        AppType::GrokBuild => &GROKBUILD_DESCRIPTOR,
        AppType::OpenCode => &OPENCODE_DESCRIPTOR,
        AppType::OpenClaw => &OPENCLAW_DESCRIPTOR,
        AppType::Hermes => &HERMES_DESCRIPTOR,
        AppType::Pi => &PI_DESCRIPTOR,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn registry_keeps_stable_order_and_identifiers() {
        let registry = builtin_app_registry();
        let identifiers: Vec<_> = registry.descriptors().map(AppDescriptor::id).collect();

        assert_eq!(
            identifiers,
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
        for descriptor in registry.descriptors() {
            assert_eq!(registry.for_app(descriptor.app()), descriptor);
        }
    }

    #[test]
    fn registry_identifiers_and_aliases_are_unique() {
        let mut identifiers = HashSet::new();
        for descriptor in builtin_app_registry().descriptors() {
            assert!(identifiers.insert(descriptor.id()));
            for alias in descriptor.aliases {
                assert!(identifiers.insert(alias));
            }
        }
    }

    #[test]
    fn registry_resolves_canonical_ids_and_existing_aliases() {
        let registry = builtin_app_registry();

        for descriptor in registry.descriptors() {
            assert_eq!(registry.find(descriptor.id()), Some(descriptor));
            for alias in descriptor.aliases {
                assert_eq!(registry.find(alias), Some(descriptor));
            }
        }

        assert_eq!(
            registry.find(" CLAUDEDESKTOP ").map(AppDescriptor::id),
            Some("claude-desktop")
        );
        assert!(registry.find("unknown").is_none());
    }

    #[test]
    fn registry_is_the_source_for_activation_and_proxy_semantics() {
        for descriptor in builtin_app_registry().descriptors() {
            assert_eq!(
                descriptor.configuration_mode() == ProviderConfigurationMode::Additive,
                descriptor.app().is_additive_mode()
            );
            assert_eq!(
                descriptor.supports(AppCapability::LocalProxy),
                descriptor.app().supports_local_proxy()
            );
        }
    }

    #[test]
    fn registry_keeps_the_complete_capability_matrix() {
        use AppCapability::{
            CommonConfiguration, LiveConfiguration, LocalProxy, Mcp, Prompts, ProviderManagement,
            Skills,
        };
        use ProviderConfigurationMode::{Additive, Switch};

        let expected: &[(&str, ProviderConfigurationMode, &[AppCapability])] = &[
            (
                "claude",
                Switch,
                &[
                    ProviderManagement,
                    LiveConfiguration,
                    CommonConfiguration,
                    LocalProxy,
                    Mcp,
                    Prompts,
                    Skills,
                ],
            ),
            (
                "claude-desktop",
                Switch,
                &[ProviderManagement, LiveConfiguration],
            ),
            (
                "codex",
                Switch,
                &[
                    ProviderManagement,
                    LiveConfiguration,
                    CommonConfiguration,
                    LocalProxy,
                    Mcp,
                    Prompts,
                    Skills,
                ],
            ),
            (
                "gemini",
                Switch,
                &[
                    ProviderManagement,
                    LiveConfiguration,
                    CommonConfiguration,
                    LocalProxy,
                    Mcp,
                    Prompts,
                    Skills,
                ],
            ),
            (
                "grokbuild",
                Switch,
                &[
                    ProviderManagement,
                    LiveConfiguration,
                    LocalProxy,
                    Mcp,
                    Prompts,
                    Skills,
                ],
            ),
            (
                "opencode",
                Additive,
                &[ProviderManagement, LiveConfiguration, Mcp, Prompts, Skills],
            ),
            (
                "openclaw",
                Additive,
                &[ProviderManagement, LiveConfiguration, Prompts],
            ),
            (
                "hermes",
                Additive,
                &[ProviderManagement, LiveConfiguration, Mcp, Prompts, Skills],
            ),
            (
                "pi",
                Additive,
                &[ProviderManagement, LiveConfiguration, Prompts, Skills],
            ),
        ];

        let registry = builtin_app_registry();
        assert_eq!(registry.descriptors().len(), expected.len());
        for (id, mode, capabilities) in expected {
            let descriptor = registry.find(id).expect("expected built-in descriptor");
            assert_eq!(descriptor.configuration_mode(), *mode, "{id}");
            assert_eq!(descriptor.capabilities(), *capabilities, "{id}");
        }
    }

    #[test]
    fn skill_capabilities_have_exactly_one_contract() {
        for descriptor in builtin_app_registry().descriptors() {
            assert_eq!(
                descriptor.supports(AppCapability::Skills),
                descriptor.skill_contract().is_some(),
                "{}",
                descriptor.id()
            );
        }
    }

    #[test]
    fn skill_contract_matrix_is_stable() {
        use crate::SkillActivationStore::{CatalogColumn, NativeDirectory};

        let actual = builtin_app_registry()
            .descriptors()
            .filter_map(|descriptor| {
                descriptor.skill_contract().map(|contract| {
                    (
                        descriptor.id(),
                        contract.activation(),
                        contract.discovery(),
                        contract.config_target(),
                    )
                })
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            [
                ("claude", CatalogColumn("enabled_claude"), NativeOnly, None),
                (
                    "codex",
                    CatalogColumn("enabled_codex"),
                    NativeAndUnified,
                    None,
                ),
                (
                    "gemini",
                    CatalogColumn("enabled_gemini"),
                    NativeAndUnified,
                    Some(GeminiSettings),
                ),
                (
                    "grokbuild",
                    CatalogColumn("enabled_grokbuild"),
                    NativeAndUnified,
                    Some(GrokConfig),
                ),
                (
                    "opencode",
                    CatalogColumn("enabled_opencode"),
                    NativeAndUnified,
                    None,
                ),
                (
                    "hermes",
                    CatalogColumn("enabled_hermes"),
                    NativeOnly,
                    Some(HermesConfig),
                ),
                ("pi", NativeDirectory, NativeAndUnified, None),
            ]
        );
    }

    #[test]
    fn skill_catalog_columns_are_unique_safe_identifiers() {
        let mut columns = HashSet::new();
        for descriptor in builtin_app_registry().descriptors() {
            let Some(column) = descriptor
                .skill_contract()
                .and_then(|contract| contract.activation().catalog_column())
            else {
                continue;
            };
            assert!(
                column.starts_with("enabled_")
                    && column
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{column}"
            );
            assert!(columns.insert(column), "duplicate column {column}");
        }
    }

    #[test]
    fn skill_config_targets_belong_to_the_declaring_app() {
        for descriptor in builtin_app_registry().descriptors() {
            let Some(target) = descriptor
                .skill_contract()
                .and_then(SkillAppContract::config_target)
            else {
                continue;
            };
            assert_eq!(
                target.logical_target().app(),
                descriptor.app().clone(),
                "{}",
                descriptor.id()
            );
        }
    }

    #[test]
    fn serialized_descriptors_expose_product_neutral_metadata() {
        let descriptor = builtin_app_registry().for_app(&AppType::ClaudeDesktop);

        assert_eq!(
            serde_json::to_value(descriptor).expect("serialize app descriptor"),
            json!({
                "id": "claude-desktop",
                "displayName": "Claude Desktop",
                "brandKey": "claude",
                "configurationMode": "switch",
                "capabilities": ["provider-management", "live-configuration"]
            })
        );
    }
}
