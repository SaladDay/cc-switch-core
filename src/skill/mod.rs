//! Application-level Skill contracts.
//!
//! The contract describes where an application's requested Skill selection
//! lives and how the application discovers installed Skills. Selection and
//! discovery are independent inputs to effective state. The read layer may
//! observe host-resolved paths; all database and filesystem writes remain in
//! later layers.

mod catalog;
mod config;
mod read;

use crate::LogicalTarget;

pub use catalog::{skill_catalog_columns, SkillCatalogEntry, SkillCatalogEntryError};
pub use read::{
    inspect_installed_skills, InstalledSkillSnapshot, SkillAppRuntime, SkillAppState,
    SkillControlReason, SkillReadError, SkillRuntime, SkillRuntimeError,
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

/// Where one application's requested per-Skill selection is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSelectionStore {
    /// The shared `skills` table stores the requested state in this column.
    CatalogColumn(SkillCatalogColumn),
    /// Presence in the application's native Skill directory stores selection.
    ///
    /// Unified discovery may still make the Skill visible independently.
    NativeDirectory,
}

impl SkillSelectionStore {
    /// Returns the shared catalog column, when selection is catalog-backed.
    pub const fn catalog_column(self) -> Option<SkillCatalogColumn> {
        match self {
            Self::CatalogColumn(column) => Some(column),
            Self::NativeDirectory => None,
        }
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
    selection_store: SkillSelectionStore,
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
            selection_store: SkillSelectionStore::CatalogColumn(SkillCatalogColumn::new(column)),
            discovery,
            config_target,
        }
    }

    pub(crate) const fn native_directory(discovery: SkillDiscovery) -> Self {
        Self {
            selection_store: SkillSelectionStore::NativeDirectory,
            discovery,
            config_target: None,
        }
    }

    /// Returns where this application's requested selection is stored.
    pub const fn selection_store(self) -> SkillSelectionStore {
        self.selection_store
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
    fn native_directory_selection_cannot_declare_a_config_target() {
        let contract = SkillAppContract::native_directory(SkillDiscovery::NativeAndUnified);

        assert_eq!(
            contract.selection_store(),
            SkillSelectionStore::NativeDirectory
        );
        assert_eq!(contract.config_target(), None);
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
