//! Built-in application adapter contracts.

use std::fmt;

use crate::{
    integration::{builtin_app_integration, builtin_app_integrations, AppIntegration},
    native_import, projection, AppDescriptor, AppType, LiveDocumentSet, LogicalTarget,
    McpAppContract, McpConfigError, McpConfigTarget, McpImport, McpServerProjection, NativeAction,
    NativeImportError, NativeImportStep, NativePlanError, NativePlanRequest, OperationPlan,
    OperationPlanError, ProviderSnapshot, SimpleProviderError, SimpleProviderFormDescriptor,
    SimpleProviderValues,
};

mod sealed {
    pub trait Sealed {}
}

/// Shared, read-only contract for a built-in application integration.
///
/// The trait is sealed because marketplace plugins use a versioned manifest
/// contract rather than Rust's unstable dynamic-library ABI.
pub trait AppAdapter: sealed::Sealed + fmt::Debug + Send + Sync {
    /// Returns the application's registry descriptor.
    fn descriptor(&self) -> &'static AppDescriptor;

    /// Returns every logical native document this adapter may manage.
    fn targets(&self) -> &'static [LogicalTarget];

    /// Returns the product-neutral simple provider form for this application.
    fn simple_provider_form(&self) -> &'static SimpleProviderFormDescriptor {
        crate::simple_provider_form(self.descriptor().app())
    }

    /// Returns this application's MCP document target, when supported.
    fn mcp_config_target(&self) -> Option<McpConfigTarget> {
        self.mcp_contract().map(|contract| contract.target())
    }

    /// Returns this application's MCP behavior, when supported.
    fn mcp_contract(&self) -> Option<&'static McpAppContract> {
        self.descriptor().mcp_contract()
    }

    /// Extracts valid unified MCP servers from an observed live document.
    fn import_mcp_servers(
        &self,
        contents: Option<&[u8]>,
    ) -> Result<Vec<McpImport>, McpConfigError> {
        crate::import_mcp_servers(self.descriptor().app(), contents)
    }

    /// Projects one MCP state change into the complete live document.
    fn project_mcp_server(
        &self,
        contents: Option<&[u8]>,
        id: &str,
        projection: McpServerProjection<'_>,
    ) -> Result<Option<String>, McpConfigError> {
        crate::project_mcp_server(self.descriptor().app(), contents, id, projection)
    }

    /// Extracts the small, shared provider field set from native settings.
    fn extract_simple_provider_values(
        &self,
        settings: &serde_json::Value,
    ) -> Result<SimpleProviderValues, SimpleProviderError> {
        crate::extract_simple_provider_values(self.descriptor().app(), settings)
    }

    /// Projects the small, shared provider field set into native settings.
    fn project_simple_provider_settings(
        &self,
        provider_name: &str,
        values: &SimpleProviderValues,
        existing: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, SimpleProviderError> {
        crate::project_simple_provider_settings(
            self.descriptor().app(),
            provider_name,
            values,
            existing,
        )
    }

    /// Returns the targets a host must observe for this native action.
    fn required_native_targets(
        &self,
        action: NativeAction,
        provider: &ProviderSnapshot,
        mode: crate::NativeProviderMode,
    ) -> Result<Vec<LogicalTarget>, NativePlanError> {
        projection::required_native_targets(
            self.descriptor().app(),
            self.targets(),
            action,
            provider,
            mode,
        )
    }

    /// Returns the targets required by a consumer-projected planning policy.
    fn required_native_targets_for_policy(
        &self,
        action: NativeAction,
        provider: &ProviderSnapshot,
        policy: &crate::NativePlanPolicy<'_>,
    ) -> Result<Vec<LogicalTarget>, NativePlanError> {
        projection::required_native_targets_for_policy(
            self.descriptor().app(),
            self.targets(),
            action,
            provider,
            policy,
        )
    }

    /// Validates that an operation plan belongs to this adapter.
    fn validate_plan(&self, plan: &OperationPlan) -> Result<(), OperationPlanError> {
        plan.validate_for(self.descriptor().app())?;
        if let Some(target) = plan
            .writes
            .iter()
            .map(|write| write.target)
            .find(|target| !self.targets().contains(target))
        {
            return Err(OperationPlanError::UndeclaredTarget { target });
        }
        Ok(())
    }

    /// Projects one native provider action into a compare-and-swap plan.
    fn plan_native(
        &self,
        request: &NativePlanRequest<'_>,
    ) -> Result<OperationPlan, NativePlanError> {
        let plan = projection::plan_native(self.descriptor().app(), request)?;
        self.validate_plan(&plan)?;
        Ok(plan)
    }

    /// Builds a plan from a typed consumer-projected policy.
    ///
    /// Multi-document policy plans must be executed with
    /// [`crate::execute_dependency_ordered_plan`] so rollback preserves their
    /// declared dependency order.
    fn plan_native_policy(
        &self,
        request: &crate::NativePolicyPlanRequest<'_>,
    ) -> Result<OperationPlan, NativePlanError> {
        let plan = projection::plan_native_policy(self.descriptor().app(), request)?;
        self.validate_plan(&plan)?;
        Ok(plan)
    }

    /// Advances a pure native import projection by one observation step.
    fn project_native_import(
        &self,
        documents: &LiveDocumentSet,
    ) -> Result<NativeImportStep, NativeImportError> {
        native_import::project_native_import(self.descriptor().app(), documents)
    }
}

impl sealed::Sealed for AppIntegration {}

impl AppAdapter for AppIntegration {
    fn descriptor(&self) -> &'static AppDescriptor {
        builtin_app_integration(AppIntegration::descriptor(self).app()).descriptor()
    }

    fn targets(&self) -> &'static [LogicalTarget] {
        AppIntegration::targets(self)
    }
}

/// Iterates over built-in adapters in registry display order.
pub fn builtin_app_adapters(
) -> impl ExactSizeIterator<Item = &'static dyn AppAdapter> + DoubleEndedIterator + Clone {
    builtin_app_integrations().map(|integration| integration as &dyn AppAdapter)
}

/// Returns the built-in adapter for an application.
pub fn builtin_app_adapter(app: &AppType) -> &'static dyn AppAdapter {
    builtin_app_integration(app)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{
        builtin_app_registry, AppCapability, ContentExpectation, PlannedWrite,
        OPERATION_CONTRACT_MAJOR,
    };

    #[derive(Debug)]
    struct TestAdapter;

    impl sealed::Sealed for TestAdapter {}

    impl AppAdapter for TestAdapter {
        fn descriptor(&self) -> &'static AppDescriptor {
            builtin_app_registry().for_app(&AppType::Claude)
        }

        fn targets(&self) -> &'static [LogicalTarget] {
            &[]
        }
    }

    #[test]
    fn every_registry_app_has_one_ordered_adapter() {
        let descriptor_ids: Vec<_> = builtin_app_registry()
            .descriptors()
            .map(AppDescriptor::id)
            .collect();
        let adapter_ids: Vec<_> = builtin_app_adapters()
            .map(|adapter| adapter.descriptor().id())
            .collect();

        assert_eq!(adapter_ids, descriptor_ids);
        for adapter in builtin_app_adapters() {
            assert_eq!(
                builtin_app_adapter(adapter.descriptor().app())
                    .descriptor()
                    .id(),
                adapter.descriptor().id()
            );
        }
    }

    #[test]
    fn adapters_own_every_logical_target_exactly_once() {
        let mut targets = HashSet::new();
        for adapter in builtin_app_adapters() {
            assert!(
                adapter
                    .descriptor()
                    .supports(AppCapability::LiveConfiguration),
                "{}",
                adapter.descriptor().id()
            );
            assert!(
                !adapter.targets().is_empty(),
                "{}",
                adapter.descriptor().id()
            );
            for target in adapter.targets() {
                assert_eq!(&target.app(), adapter.descriptor().app());
                assert!(targets.insert(*target), "duplicate target {target:?}");
            }
        }

        assert_eq!(
            targets,
            LogicalTarget::ALL.into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn logical_target_wire_and_ownership_matrix_is_stable() {
        use crate::ConfigFormat::{Env, Json, Json5, Toml, Yaml};

        let expected = [
            (
                LogicalTarget::ClaudeSettings,
                "claudeSettings",
                AppType::Claude,
                Json,
                false,
            ),
            (
                LogicalTarget::ClaudeDesktopNormalConfig,
                "claudeDesktopNormalConfig",
                AppType::ClaudeDesktop,
                Json,
                false,
            ),
            (
                LogicalTarget::ClaudeDesktopThreepConfig,
                "claudeDesktopThreepConfig",
                AppType::ClaudeDesktop,
                Json,
                false,
            ),
            (
                LogicalTarget::ClaudeDesktopProfile,
                "claudeDesktopProfile",
                AppType::ClaudeDesktop,
                Json,
                true,
            ),
            (
                LogicalTarget::ClaudeDesktopMeta,
                "claudeDesktopMeta",
                AppType::ClaudeDesktop,
                Json,
                false,
            ),
            (
                LogicalTarget::CodexAuth,
                "codexAuth",
                AppType::Codex,
                Json,
                true,
            ),
            (
                LogicalTarget::CodexConfig,
                "codexConfig",
                AppType::Codex,
                Toml,
                false,
            ),
            (
                LogicalTarget::CodexModelCatalog,
                "codexModelCatalog",
                AppType::Codex,
                Json,
                true,
            ),
            (
                LogicalTarget::GeminiEnv,
                "geminiEnv",
                AppType::Gemini,
                Env,
                false,
            ),
            (
                LogicalTarget::GeminiSettings,
                "geminiSettings",
                AppType::Gemini,
                Json,
                false,
            ),
            (
                LogicalTarget::GrokConfig,
                "grokConfig",
                AppType::GrokBuild,
                Toml,
                false,
            ),
            (
                LogicalTarget::OpenCodeConfig,
                "openCodeConfig",
                AppType::OpenCode,
                Json,
                false,
            ),
            (
                LogicalTarget::OpenClawConfig,
                "openClawConfig",
                AppType::OpenClaw,
                Json5,
                false,
            ),
            (
                LogicalTarget::HermesConfig,
                "hermesConfig",
                AppType::Hermes,
                Yaml,
                false,
            ),
            (
                LogicalTarget::PiModels,
                "piModels",
                AppType::Pi,
                Json,
                false,
            ),
        ];

        assert_eq!(
            expected.iter().map(|row| row.0).collect::<Vec<_>>(),
            LogicalTarget::ALL
        );
        for (target, wire_name, app, format, removable) in expected {
            assert_eq!(
                serde_json::to_value(target).expect("serialize logical target"),
                wire_name
            );
            assert_eq!(target.app(), app);
            assert_eq!(target.format(), format);
            assert_eq!(target.allows_removal(), removable);
            assert!(builtin_app_adapter(&app).targets().contains(&target));
        }
    }

    #[test]
    fn every_adapter_accepts_its_declared_targets() {
        for adapter in builtin_app_adapters() {
            for target in adapter.targets() {
                let plan = OperationPlan {
                    contract_major: OPERATION_CONTRACT_MAJOR,
                    app_id: adapter.descriptor().id().to_owned(),
                    writes: vec![PlannedWrite {
                        target: *target,
                        expected: ContentExpectation::Missing,
                        contents: Some("validity is checked by the host".to_owned()),
                    }],
                };

                assert_eq!(adapter.validate_plan(&plan), Ok(()), "{target:?}");
            }
        }
    }

    #[test]
    fn adapter_validation_rejects_wrong_apps_and_undeclared_targets() {
        let wrong_app = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "codex".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::CodexConfig,
                expected: ContentExpectation::Missing,
                contents: Some(String::new()),
            }],
        };
        assert!(matches!(
            builtin_app_adapter(&AppType::Claude).validate_plan(&wrong_app),
            Err(OperationPlanError::WrongApp { .. })
        ));

        let adapter_without_targets = TestAdapter;
        let undeclared = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::ClaudeSettings,
                expected: ContentExpectation::Missing,
                contents: Some(String::new()),
            }],
        };
        assert!(matches!(
            adapter_without_targets.validate_plan(&undeclared),
            Err(OperationPlanError::UndeclaredTarget { .. })
        ));
    }
}
