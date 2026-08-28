//! Built-in application adapter contracts.

use std::fmt;

use crate::{
    builtin_app_registry, AppDescriptor, AppType, LogicalTarget, OperationPlan, OperationPlanError,
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
}

#[derive(Debug)]
struct BuiltinAdapter {
    app: AppType,
    targets: &'static [LogicalTarget],
}

impl sealed::Sealed for BuiltinAdapter {}

impl AppAdapter for BuiltinAdapter {
    fn descriptor(&self) -> &'static AppDescriptor {
        builtin_app_registry().for_app(&self.app)
    }

    fn targets(&self) -> &'static [LogicalTarget] {
        self.targets
    }
}

static CLAUDE_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::Claude,
    targets: &[LogicalTarget::ClaudeSettings],
};

static CLAUDE_DESKTOP_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::ClaudeDesktop,
    targets: &[
        LogicalTarget::ClaudeDesktopNormalConfig,
        LogicalTarget::ClaudeDesktopThreepConfig,
        LogicalTarget::ClaudeDesktopProfile,
        LogicalTarget::ClaudeDesktopMeta,
    ],
};

static CODEX_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::Codex,
    targets: &[
        LogicalTarget::CodexAuth,
        LogicalTarget::CodexConfig,
        LogicalTarget::CodexModelCatalog,
    ],
};

static GEMINI_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::Gemini,
    targets: &[LogicalTarget::GeminiEnv, LogicalTarget::GeminiSettings],
};

static GROKBUILD_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::GrokBuild,
    targets: &[LogicalTarget::GrokConfig],
};

static OPENCODE_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::OpenCode,
    targets: &[LogicalTarget::OpenCodeConfig],
};

static OPENCLAW_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::OpenClaw,
    targets: &[LogicalTarget::OpenClawConfig],
};

static HERMES_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::Hermes,
    targets: &[LogicalTarget::HermesConfig],
};

static PI_ADAPTER: BuiltinAdapter = BuiltinAdapter {
    app: AppType::Pi,
    targets: &[LogicalTarget::PiModels],
};

static BUILTIN_ADAPTERS: [&dyn AppAdapter; 9] = [
    &CLAUDE_ADAPTER,
    &CLAUDE_DESKTOP_ADAPTER,
    &CODEX_ADAPTER,
    &GEMINI_ADAPTER,
    &GROKBUILD_ADAPTER,
    &OPENCODE_ADAPTER,
    &OPENCLAW_ADAPTER,
    &HERMES_ADAPTER,
    &PI_ADAPTER,
];

/// Iterates over built-in adapters in registry display order.
pub fn builtin_app_adapters(
) -> impl ExactSizeIterator<Item = &'static dyn AppAdapter> + DoubleEndedIterator + Clone {
    BUILTIN_ADAPTERS.iter().copied()
}

/// Returns the built-in adapter for an application.
pub fn builtin_app_adapter(app: &AppType) -> &'static dyn AppAdapter {
    match app {
        AppType::Claude => &CLAUDE_ADAPTER,
        AppType::ClaudeDesktop => &CLAUDE_DESKTOP_ADAPTER,
        AppType::Codex => &CODEX_ADAPTER,
        AppType::Gemini => &GEMINI_ADAPTER,
        AppType::GrokBuild => &GROKBUILD_ADAPTER,
        AppType::OpenCode => &OPENCODE_ADAPTER,
        AppType::OpenClaw => &OPENCLAW_ADAPTER,
        AppType::Hermes => &HERMES_ADAPTER,
        AppType::Pi => &PI_ADAPTER,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{AppCapability, ContentExpectation, PlannedWrite, OPERATION_CONTRACT_MAJOR};

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
            CLAUDE_ADAPTER.validate_plan(&wrong_app),
            Err(OperationPlanError::WrongApp { .. })
        ));

        let adapter_without_targets = BuiltinAdapter {
            app: AppType::Claude,
            targets: &[],
        };
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
