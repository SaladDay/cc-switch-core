//! Application-level Skill contracts.
//!
//! The contract describes where an application's requested Skill selection
//! lives and how the application discovers installed Skills. Selection and
//! discovery are independent inputs to effective state. Hosts resolve paths,
//! own database transactions and perform logical document I/O. Core owns the
//! shared observation, projection, reference and rollback rules.

mod catalog;
mod config;
mod read;
mod reference;
mod write;

use crate::LogicalTarget;

pub use catalog::{skill_catalog_columns, SkillCatalogEntry, SkillCatalogEntryError};
pub use read::{
    inspect_installed_skills, InstalledSkillSnapshot, SkillAppRuntime, SkillAppState,
    SkillControlReason, SkillReadError, SkillRuntime, SkillRuntimeError,
};
pub use reference::{
    apply_skill_reference, SkillReferenceError, SkillReferencePlan, SkillReferenceReceipt,
};
pub use write::{
    execute_skill_live_plan, prepare_skill_reconciliation, prepare_skill_switch,
    SkillCatalogChange, SkillCatalogDecision, SkillCatalogGuard, SkillLiveExecutionError,
    SkillLiveFailure, SkillLiveReceipt, SkillLiveRollbackError, SkillLiveRollbackFailure,
    SkillPrepareError, SkillSwitchPlan, SkillWriteOrder,
};

/// A schema-backed `skills` selection column declared by Core.
///
/// Hosts can read its identifier but cannot construct arbitrary columns.
///
/// ```compile_fail
/// use cc_switch_core::SkillCatalogColumn;
/// let _ = SkillCatalogColumn("enabled_unknown");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkillCatalogColumn(&'static str);

impl SkillCatalogColumn {
    pub(crate) const fn new(column: &'static str) -> Self {
        Self(column)
    }

    /// Returns the fixed database identifier.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// How an application discovers installed Skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillDiscovery {
    /// The application reads only its native Skill directory.
    NativeOnly,
    /// The application also reads the shared `~/.agents/skills` directory.
    NativeAndUnified,
}

impl SkillDiscovery {
    /// Returns whether the application also reads the unified Skill directory.
    pub const fn reads_unified_store(self) -> bool {
        matches!(self, Self::NativeAndUnified)
    }
}

/// Native document containing a supported per-Skill control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillConfigTarget {
    GeminiSettings,
    GrokConfig,
    HermesConfig,
}

impl SkillConfigTarget {
    /// Returns the logical document edited by this native control.
    pub const fn logical_target(self) -> LogicalTarget {
        match self {
            Self::GeminiSettings => LogicalTarget::GeminiSettings,
            Self::GrokConfig => LogicalTarget::GrokConfig,
            Self::HermesConfig => LogicalTarget::HermesConfig,
        }
    }
}

/// Product-neutral Skill behavior declared by an application descriptor.
///
/// Selection records the requested app-specific state. Discovery describes
/// where the app can actually see Skills; a service layer resolves both into
/// effective state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkillAppContract {
    catalog_column: SkillCatalogColumn,
    discovery: SkillDiscovery,
    config_target: Option<SkillConfigTarget>,
}

impl SkillAppContract {
    pub(crate) const fn catalog(
        column: &'static str,
        discovery: SkillDiscovery,
        config_target: Option<SkillConfigTarget>,
    ) -> Self {
        Self {
            catalog_column: SkillCatalogColumn::new(column),
            discovery,
            config_target,
        }
    }

    /// Returns the shared-catalog column that owns requested state.
    pub const fn catalog_column(self) -> SkillCatalogColumn {
        self.catalog_column
    }

    /// Returns how this application discovers installed Skills.
    pub const fn discovery(self) -> SkillDiscovery {
        self.discovery
    }

    /// Returns the native per-Skill control, when one is supported.
    pub const fn config_target(self) -> Option<SkillConfigTarget> {
        self.config_target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_contract_has_a_catalog_selection() {
        let contract =
            SkillAppContract::catalog("enabled_test", SkillDiscovery::NativeAndUnified, None);

        assert_eq!(contract.catalog_column().as_str(), "enabled_test");
    }

    #[test]
    fn config_targets_map_to_their_owned_documents() {
        assert_eq!(
            SkillConfigTarget::GeminiSettings.logical_target(),
            LogicalTarget::GeminiSettings
        );
        assert_eq!(
            SkillConfigTarget::GrokConfig.logical_target(),
            LogicalTarget::GrokConfig
        );
        assert_eq!(
            SkillConfigTarget::HermesConfig.logical_target(),
            LogicalTarget::HermesConfig
        );
    }
}
