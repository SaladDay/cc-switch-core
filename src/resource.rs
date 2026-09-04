//! Product-neutral configuration-resource declarations.
//!
//! Core declares how native documents relate to a host-supplied application
//! config root. Hosts retain all settings, environment, platform-path, and I/O
//! behavior so existing products do not have to change path semantics.

/// Location of one native configuration document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NativeResourcePath {
    /// Paths relative to the application's resolved configuration root.
    ///
    /// Hosts must use `preferred` when it exists, otherwise the first existing
    /// fallback in order. They create `preferred` only when none exists.
    ConfigRootRelative {
        preferred: &'static str,
        fallbacks: &'static [&'static str],
    },
    /// A platform-specific location supplied by the host.
    HostDefined,
}

impl NativeResourcePath {
    pub(crate) const fn relative(preferred: &'static str) -> Self {
        Self::ConfigRootRelative {
            preferred,
            fallbacks: &[],
        }
    }

    pub(crate) const fn relative_with_fallbacks(
        preferred: &'static str,
        fallbacks: &'static [&'static str],
    ) -> Self {
        Self::ConfigRootRelative {
            preferred,
            fallbacks,
        }
    }

    /// Returns the preferred path and ordered legacy fallbacks when this
    /// resource is relative to an application's config root.
    pub const fn config_root_relative(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::ConfigRootRelative {
                preferred,
                fallbacks,
            } => Some((preferred, fallbacks)),
            Self::HostDefined => None,
        }
    }
}
