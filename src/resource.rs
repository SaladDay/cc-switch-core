//! Product-neutral configuration-resource declarations.
//!
//! Core declares how native documents relate to a host-supplied application
//! config root. Hosts retain all settings, environment, platform-path, and I/O
//! behavior so existing products do not have to change path semantics.

/// Default location of an application's native configuration root.
///
/// Hosts retain settings and environment-variable precedence. `HomeRelative`
/// only declares the common default below the resolved user home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NativeConfigRoot {
    HomeRelative { path: &'static str },
    HostDefined,
}

impl NativeConfigRoot {
    pub(crate) const fn home_relative(path: &'static str) -> Self {
        Self::HomeRelative { path }
    }

    /// Returns the common path relative to the user home directory, when one
    /// exists. Platform-specific roots remain host-defined.
    pub const fn home_relative_path(self) -> Option<&'static str> {
        match self {
            Self::HomeRelative { path } => Some(path),
            Self::HostDefined => None,
        }
    }
}

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
