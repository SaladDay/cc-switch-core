//! Built-in application metadata and capability registry.

use serde::Serialize;

use crate::{
    integration::{builtin_app_integration, builtin_app_integrations},
    AppType, McpAppContract, SkillAppContract,
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
    pub(crate) const fn new(
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

    pub(crate) const fn with_mcp(mut self, contract: &'static McpAppContract) -> Self {
        self.mcp_contract = Some(contract);
        self
    }

    pub(crate) const fn with_skills(mut self, contract: SkillAppContract) -> Self {
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
        builtin_app_integrations().map(|integration| integration.descriptor())
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

static BUILTIN_APP_REGISTRY: BuiltinAppRegistry = BuiltinAppRegistry { _private: () };

pub(crate) fn descriptor_for(app: &AppType) -> &'static AppDescriptor {
    builtin_app_integration(app).descriptor()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;
    use crate::{
        builtin_app_adapter, LogicalTarget, McpConfigResource, NativeResourcePath,
        SkillConfigTarget::{GeminiSettings, GrokConfig, HermesConfig},
        SkillDiscovery::{NativeAndUnified, NativeOnly},
    };

    fn is_safe_relative_path(path: &str) -> bool {
        path.is_ascii()
            && !path
                .chars()
                .any(|character| matches!(character, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            && path.split(['/', '\\']).all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.ends_with('.')
                    && !part.ends_with(' ')
                    && !is_windows_device_name(part)
                    && !part.bytes().any(|byte| byte.is_ascii_control())
            })
    }

    fn is_windows_device_name(part: &str) -> bool {
        let stem = part.split('.').next().unwrap_or_default();
        matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "CONIN$"
                | "CONOUT$"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    }

    fn path_identity(path: &str) -> String {
        path.replace('\\', "/").to_ascii_lowercase()
    }

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
    fn registry_declares_complete_host_resource_contracts() {
        fn assert_safe_resource(resource: NativeResourcePath, app_id: &str) {
            match resource {
                NativeResourcePath::ConfigRootRelative {
                    preferred,
                    fallbacks,
                } => {
                    assert!(is_safe_relative_path(preferred), "{app_id}");
                    let mut paths = HashSet::from([path_identity(preferred)]);
                    assert!(
                        fallbacks
                            .iter()
                            .all(|path| is_safe_relative_path(path)
                                && paths.insert(path_identity(path))),
                        "{app_id}"
                    );
                }
                NativeResourcePath::HostDefined => {}
            }
        }

        for descriptor in builtin_app_registry().descriptors() {
            let adapter = builtin_app_adapter(descriptor.app());
            for target in adapter.targets() {
                assert_safe_resource(target.resource_path(), descriptor.id());
            }
            if let Some(contract) = descriptor.skill_contract() {
                assert_safe_resource(contract.native_resource(), descriptor.id());
            }

            if let Some(McpConfigResource::LogicalTarget(target)) = descriptor
                .mcp_contract()
                .map(|contract| contract.resource())
            {
                assert_eq!(target.app(), descriptor.app().clone());
                assert!(adapter.targets().contains(&target));
            }
        }
    }

    #[test]
    fn resource_paths_reject_cross_platform_aliases() {
        for path in [
            "../outside",
            "/absolute",
            r"C:\absolute",
            "NUL.json",
            "nested/COM1.log",
            "trailing. ",
            "unicode-配置.json",
        ] {
            assert!(!is_safe_relative_path(path), "{path}");
        }
        for path in ["settings.json", ".env", ".config/opencode/config.json"] {
            assert!(is_safe_relative_path(path), "{path}");
        }
    }

    #[test]
    fn native_resource_matrix_is_stable() {
        let relative = NativeResourcePath::relative;
        let host_defined = NativeResourcePath::HostDefined;
        let mut checked = Vec::new();
        macro_rules! check {
            ($target:expr, $resource:expr) => {{
                checked.push($target);
                assert_eq!($target.resource_path(), $resource);
            }};
        }
        check!(
            LogicalTarget::ClaudeSettings,
            NativeResourcePath::relative_with_fallbacks("settings.json", &["claude.json"])
        );
        check!(LogicalTarget::ClaudeDesktopNormalConfig, host_defined);
        check!(LogicalTarget::ClaudeDesktopThreepConfig, host_defined);
        check!(LogicalTarget::ClaudeDesktopProfile, host_defined);
        check!(LogicalTarget::ClaudeDesktopMeta, host_defined);
        check!(LogicalTarget::CodexAuth, relative("auth.json"));
        check!(LogicalTarget::CodexConfig, relative("config.toml"));
        check!(
            LogicalTarget::CodexModelCatalog,
            relative(crate::codex::MODEL_CATALOG_FILENAME)
        );
        check!(LogicalTarget::GeminiEnv, relative(".env"));
        check!(LogicalTarget::GeminiSettings, relative("settings.json"));
        check!(LogicalTarget::GrokConfig, relative("config.toml"));
        check!(LogicalTarget::OpenCodeConfig, relative("opencode.json"));
        check!(LogicalTarget::OpenClawConfig, relative("openclaw.json"));
        check!(LogicalTarget::HermesConfig, relative("config.yaml"));
        check!(LogicalTarget::PiModels, relative("models.json"));
        assert_eq!(checked, LogicalTarget::ALL);
    }

    #[test]
    fn mcp_resource_matrix_is_stable() {
        fn resource(id: &str) -> McpConfigResource {
            builtin_app_registry()
                .find(id)
                .and_then(AppDescriptor::mcp_contract)
                .map(|contract| contract.resource())
                .expect("registered MCP resource")
        }
        let target = McpConfigResource::LogicalTarget;
        let mut checked = Vec::new();
        macro_rules! check {
            ($app:literal, $resource:expr) => {{
                checked.push($app);
                assert_eq!(resource($app), $resource);
            }};
        }
        check!("claude", McpConfigResource::HostDefined);
        check!("codex", target(LogicalTarget::CodexConfig));
        check!("gemini", target(LogicalTarget::GeminiSettings));
        check!("grokbuild", target(LogicalTarget::GrokConfig));
        check!("opencode", target(LogicalTarget::OpenCodeConfig));
        check!("hermes", target(LogicalTarget::HermesConfig));
        assert_eq!(
            checked,
            builtin_app_registry()
                .descriptors()
                .filter(|descriptor| descriptor.mcp_contract().is_some())
                .map(AppDescriptor::id)
                .collect::<Vec<_>>()
        );
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
    fn mcp_catalog_columns_follow_the_registry() {
        let expected = [
            ("claude", "enabled_claude"),
            ("codex", "enabled_codex"),
            ("gemini", "enabled_gemini"),
            ("grokbuild", "enabled_grokbuild"),
            ("opencode", "enabled_opencode"),
            ("hermes", "enabled_hermes"),
        ];
        let actual = builtin_app_registry()
            .descriptors()
            .filter_map(|descriptor| {
                descriptor
                    .mcp_contract()
                    .map(|contract| (descriptor.id(), contract.catalog_column().as_str()))
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(
            crate::mcp_catalog_columns()
                .map(crate::McpCatalogColumn::as_str)
                .collect::<Vec<_>>(),
            expected.map(|(_, column)| column)
        );

        let mut columns = HashSet::new();
        for column in crate::mcp_catalog_columns() {
            assert!(
                column.as_str().starts_with("enabled_")
                    && column
                        .as_str()
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{}",
                column.as_str()
            );
            assert!(
                columns.insert(column),
                "duplicate column {}",
                column.as_str()
            );
        }
    }

    #[test]
    fn skill_contract_matrix_is_stable() {
        let catalog = crate::SkillCatalogColumn::new;

        let actual = builtin_app_registry()
            .descriptors()
            .filter_map(|descriptor| {
                descriptor.skill_contract().map(|contract| {
                    (
                        descriptor.id(),
                        contract.catalog_column(),
                        contract.discovery(),
                        contract.config_target(),
                        contract.native_resource(),
                    )
                })
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            [
                (
                    "claude",
                    catalog("enabled_claude"),
                    NativeOnly,
                    None,
                    NativeResourcePath::relative("skills")
                ),
                (
                    "codex",
                    catalog("enabled_codex"),
                    NativeAndUnified,
                    None,
                    NativeResourcePath::relative("skills")
                ),
                (
                    "gemini",
                    catalog("enabled_gemini"),
                    NativeAndUnified,
                    Some(GeminiSettings),
                    NativeResourcePath::relative("skills"),
                ),
                (
                    "grokbuild",
                    catalog("enabled_grokbuild"),
                    NativeAndUnified,
                    Some(GrokConfig),
                    NativeResourcePath::relative("skills"),
                ),
                (
                    "opencode",
                    catalog("enabled_opencode"),
                    NativeAndUnified,
                    None,
                    NativeResourcePath::relative("skills"),
                ),
                (
                    "hermes",
                    catalog("enabled_hermes"),
                    NativeOnly,
                    Some(HermesConfig),
                    NativeResourcePath::relative("skills"),
                ),
                (
                    "pi",
                    catalog("enabled_pi"),
                    NativeAndUnified,
                    None,
                    NativeResourcePath::relative("skills")
                ),
            ]
        );
    }

    #[test]
    fn skill_catalog_columns_are_unique_safe_identifiers() {
        let mut columns = HashSet::new();
        for descriptor in builtin_app_registry().descriptors() {
            let Some(column) = descriptor
                .skill_contract()
                .map(SkillAppContract::catalog_column)
            else {
                continue;
            };
            assert!(
                column.as_str().starts_with("enabled_")
                    && column
                        .as_str()
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{}",
                column.as_str()
            );
            assert!(
                columns.insert(column),
                "duplicate column {}",
                column.as_str()
            );
        }
    }

    #[test]
    fn pi_selection_is_catalog_backed_and_discovery_is_independent() {
        let contract = builtin_app_registry()
            .for_app(&AppType::Pi)
            .skill_contract()
            .expect("Pi supports Skills");

        assert_eq!(contract.catalog_column().as_str(), "enabled_pi");
        assert!(contract.discovery().reads_unified_store());
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
