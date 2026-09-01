//! Application-level Skill contracts.
//!
//! The contract describes where an application's activation state lives and
//! how the application discovers installed Skills. Filesystem observation and
//! writes are added by higher layers; this module performs no I/O.

use crate::LogicalTarget;

/// Where one application's per-Skill activation state is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillActivationStore {
    /// The shared `skills` table stores the requested state in this column.
    CatalogColumn(&'static str),
    /// Presence in the application's native Skill directory is the state.
    NativeDirectory,
}

impl SkillActivationStore {
    /// Returns the shared catalog column, when activation is catalog-backed.
    pub const fn catalog_column(self) -> Option<&'static str> {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkillAppContract {
    activation: SkillActivationStore,
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
            activation: SkillActivationStore::CatalogColumn(column),
            discovery,
            config_target,
        }
    }

    pub(crate) const fn native_directory(discovery: SkillDiscovery) -> Self {
        Self {
            activation: SkillActivationStore::NativeDirectory,
            discovery,
            config_target: None,
        }
    }

    /// Returns where this application's activation state is stored.
    pub const fn activation(self) -> SkillActivationStore {
        self.activation
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
    fn native_directory_activation_cannot_declare_a_config_target() {
        let contract = SkillAppContract::native_directory(SkillDiscovery::NativeAndUnified);

        assert_eq!(contract.activation(), SkillActivationStore::NativeDirectory);
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
