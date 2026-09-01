use std::{
    collections::{HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::{
    builtin_app_registry, AppType, ObservedDocument, SkillConfigTarget, SkillDiscovery,
    MAX_OPERATION_CONTENT_BYTES,
};

use super::{
    config::{parse_native_controls, NativeSkillControl, NativeSkillControls},
    reference::{inspect_skill_reference, SkillReferenceObservation},
    SkillCatalogEntry,
};

const MAX_SKILL_CATALOG_ENTRIES: usize = 10_000;
const MAX_SKILL_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SKILL_SNAPSHOT_ENTRIES: usize = 100_000;
const MAX_SKILL_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_HERMES_PLATFORM_BYTES: usize = 128;

/// Host-resolved runtime inputs for one application.
///
/// Path overrides and platform-specific resolution remain host-owned. Core
/// owns the common observation and state policy once those paths are supplied.
/// Observation accepts roots that do not exist yet. Before executing a write,
/// the host must create the selected native root and Core state root as real
/// directories while holding the shared live-config lock.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillAppRuntime {
    app: AppType,
    native_root: PathBuf,
    state_root: PathBuf,
    config: Option<ObservedDocument>,
    hermes_platform: Option<String>,
}

impl SkillAppRuntime {
    pub fn try_new(
        app: AppType,
        native_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        config: Option<ObservedDocument>,
    ) -> Result<Self, SkillRuntimeError> {
        let native_root = native_root.into();
        if !native_root.is_absolute() {
            return Err(SkillRuntimeError::RelativeRoot { path: native_root });
        }
        let state_root = state_root.into();
        if !state_root.is_absolute() {
            return Err(SkillRuntimeError::RelativeRoot { path: state_root });
        }
        let contract = builtin_app_registry()
            .for_app(&app)
            .skill_contract()
            .ok_or_else(|| SkillRuntimeError::UnsupportedApp {
                app: app.as_str().to_owned(),
            })?;
        match (contract.config_target(), config.as_ref()) {
            (None, None) => {}
            (None, Some(document)) => {
                return Err(SkillRuntimeError::UnexpectedConfig {
                    app: app.as_str().to_owned(),
                    target: document.target(),
                })
            }
            (Some(target), None) => {
                return Err(SkillRuntimeError::MissingConfig {
                    app: app.as_str().to_owned(),
                    target,
                })
            }
            (Some(target), Some(document)) => {
                let expected = target.logical_target();
                if document.target() != expected {
                    return Err(SkillRuntimeError::WrongConfig {
                        app: app.as_str().to_owned(),
                        expected,
                        actual: document.target(),
                    });
                }
                if !document.is_observed() {
                    return Err(SkillRuntimeError::UnobservedConfig { target: expected });
                }
                if document
                    .contents()
                    .is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES)
                {
                    return Err(SkillRuntimeError::ConfigTooLarge {
                        target: expected,
                        limit: MAX_OPERATION_CONTENT_BYTES,
                    });
                }
            }
        }
        Ok(Self {
            app,
            native_root,
            state_root,
            config,
            hermes_platform: None,
        })
    }

    /// Selects the active Hermes gateway platform (for example `telegram`).
    ///
    /// Without this context, Core reports the global Hermes Skill state and
    /// leaves platform-specific disables out of the snapshot.
    pub fn try_with_hermes_platform(
        mut self,
        platform: impl Into<String>,
    ) -> Result<Self, SkillRuntimeError> {
        if self.app != AppType::Hermes {
            return Err(SkillRuntimeError::UnexpectedHermesPlatform {
                app: self.app.as_str().to_owned(),
            });
        }
        let platform = platform.into();
        if platform.is_empty()
            || platform.trim() != platform
            || platform.len() > MAX_HERMES_PLATFORM_BYTES
            || platform.chars().any(char::is_control)
        {
            return Err(SkillRuntimeError::InvalidHermesPlatform);
        }
        self.hermes_platform = Some(platform);
        Ok(self)
    }

    pub fn app(&self) -> &AppType {
        &self.app
    }

    pub fn native_root(&self) -> &Path {
        &self.native_root
    }

    /// Returns the host-resolved private state directory on the native root's
    /// filesystem. Core owns only entries inside this directory.
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Returns the active Hermes gateway platform, when the host supplied it.
    pub fn hermes_platform(&self) -> Option<&str> {
        self.hermes_platform.as_deref()
    }

    pub(super) fn config_document(&self) -> Option<&ObservedDocument> {
        self.config.as_ref()
    }
}

impl fmt::Debug for SkillAppRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillAppRuntime")
            .field("app", &self.app)
            .field("native_root", &self.native_root)
            .field("state_root", &self.state_root)
            .field("config", &self.config)
            .field("hermes_platform", &self.hermes_platform)
            .finish()
    }
}

/// Complete read-only runtime context for a requested set of applications.
///
/// Missing roots are valid for read-only snapshots. Hosts must create the
/// native and state roots before applying a prepared reference plan; Core does
/// not create host-owned directory trees.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillRuntime {
    source_root: PathBuf,
    unified_root: PathBuf,
    apps: Vec<SkillAppRuntime>,
}

impl SkillRuntime {
    pub fn try_new(
        source_root: impl Into<PathBuf>,
        unified_root: impl Into<PathBuf>,
        apps: impl IntoIterator<Item = SkillAppRuntime>,
    ) -> Result<Self, SkillRuntimeError> {
        let source_root = source_root.into();
        if !source_root.is_absolute() {
            return Err(SkillRuntimeError::RelativeRoot { path: source_root });
        }
        let unified_root = unified_root.into();
        if !unified_root.is_absolute() {
            return Err(SkillRuntimeError::RelativeRoot { path: unified_root });
        }
        let mut supplied = HashMap::new();
        for runtime in apps {
            let app = runtime.app.clone();
            if supplied.insert(app.clone(), runtime).is_some() {
                return Err(SkillRuntimeError::DuplicateApp {
                    app: app.as_str().to_owned(),
                });
            }
        }
        if supplied.is_empty() {
            return Err(SkillRuntimeError::NoApps);
        }
        let apps = builtin_app_registry()
            .descriptors()
            .filter_map(|descriptor| supplied.remove(descriptor.app()))
            .collect::<Vec<_>>();
        debug_assert!(
            supplied.is_empty(),
            "Skill runtimes use built-in applications"
        );
        validate_distinct_roots(&source_root, &unified_root, &apps)?;

        Ok(Self {
            source_root,
            unified_root,
            apps,
        })
    }

    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub fn unified_root(&self) -> &Path {
        &self.unified_root
    }

    pub fn apps(
        &self,
    ) -> impl ExactSizeIterator<Item = &SkillAppRuntime> + DoubleEndedIterator + Clone {
        self.apps.iter()
    }

    pub(super) fn app_runtime(&self, app: &AppType) -> Option<&SkillAppRuntime> {
        self.apps.iter().find(|runtime| runtime.app() == app)
    }
}

fn validate_distinct_roots(
    source_root: &Path,
    unified_root: &Path,
    apps: &[SkillAppRuntime],
) -> Result<(), SkillRuntimeError> {
    let source = resolve_root(source_root)?;
    let unified = resolve_root(unified_root)?;
    let same_declared_root = source_root == unified_root;
    if roots_overlap(&source, &unified) && !(same_declared_root && source == unified) {
        return Err(SkillRuntimeError::OverlappingRoots {
            left: source_root.to_owned(),
            right: unified_root.to_owned(),
        });
    }
    let mut resolved_apps: Vec<(&Path, PathBuf)> = Vec::with_capacity(apps.len() * 2);
    for app in apps {
        for path in [&app.native_root, &app.state_root] {
            let resolved = resolve_root(path)?;
            for (other_path, other) in std::iter::once((source_root, &source))
                .chain(std::iter::once((unified_root, &unified)))
                .chain(
                    resolved_apps
                        .iter()
                        .map(|(path, resolved)| (*path, resolved)),
                )
            {
                if roots_overlap(&resolved, other) {
                    return Err(SkillRuntimeError::OverlappingRoots {
                        left: path.to_owned(),
                        right: other_path.to_owned(),
                    });
                }
            }
            resolved_apps.push((path, resolved));
        }
    }
    Ok(())
}

pub(super) fn roots_overlap(left: &Path, right: &Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let left = comparable_path(left);
        let right = comparable_path(right);
        left == right || left.starts_with(&right) || right.starts_with(&left)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right || left.starts_with(right) || right.starts_with(left)
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn comparable_path(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

pub(super) fn resolve_root(path: &Path) -> Result<PathBuf, SkillRuntimeError> {
    let mut last_missing = None;
    for ancestor in path.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                let suffix = path.strip_prefix(ancestor).map_err(|error| {
                    SkillRuntimeError::RootResolution {
                        path: path.to_owned(),
                        message: error.to_string(),
                    }
                })?;
                for component in suffix.components() {
                    match component {
                        Component::Normal(name) => resolved.push(name),
                        Component::CurDir => {}
                        Component::ParentDir => {
                            if !resolved.pop() {
                                return Err(SkillRuntimeError::RootResolution {
                                    path: path.to_owned(),
                                    message: "path escapes its filesystem root".to_owned(),
                                });
                            }
                        }
                        Component::RootDir | Component::Prefix(_) => {
                            return Err(SkillRuntimeError::RootResolution {
                                path: path.to_owned(),
                                message: "path suffix is not relative".to_owned(),
                            })
                        }
                    }
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_missing = Some(error)
            }
            Err(error) => {
                return Err(SkillRuntimeError::RootResolution {
                    path: ancestor.to_owned(),
                    message: error.to_string(),
                })
            }
        }
    }
    Err(SkillRuntimeError::RootResolution {
        path: path.to_owned(),
        message: last_missing
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no filesystem ancestor could be resolved".to_owned()),
    })
}

impl fmt::Debug for SkillRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillRuntime")
            .field("source_root", &self.source_root)
            .field("unified_root", &self.unified_root)
            .field("apps", &self.apps)
            .finish()
    }
}

/// One installed Skill with observed state for every requested application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSkillSnapshot {
    id: String,
    name: String,
    description: Option<String>,
    directory: String,
    apps: Vec<SkillAppState>,
}

impl InstalledSkillSnapshot {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn apps(
        &self,
    ) -> impl ExactSizeIterator<Item = &SkillAppState> + DoubleEndedIterator + Clone {
        self.apps.iter()
    }
}

/// Requested selection and observed effective state for one application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAppState {
    app: AppType,
    selected: Option<bool>,
    enabled: Option<bool>,
    writable: bool,
    can_enable: bool,
    can_disable: bool,
    reason: Option<SkillControlReason>,
}

impl SkillAppState {
    pub fn app(&self) -> &AppType {
        &self.app
    }

    /// Returns the persisted shared-catalog request.
    pub fn selected(&self) -> Option<bool> {
        self.selected
    }

    /// Returns whether this catalog Skill is effectively visible to the app.
    /// `None` means Core could not safely identify the native entry.
    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Returns whether the opposite effective state can be requested safely.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Returns whether Core can safely prepare an enable request.
    pub fn can_enable(&self) -> bool {
        self.can_enable
    }

    /// Returns whether Core can safely prepare a disable request.
    pub fn can_disable(&self) -> bool {
        self.can_disable
    }

    pub fn reason(&self) -> Option<SkillControlReason> {
        self.reason
    }
}

/// Why a Skill switch is read-only or its effective state is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum SkillControlReason {
    MissingSource,
    InvalidSource,
    NativeConflict,
    UnifiedConflict,
    ObservationFailed,
    InvalidConfiguration,
    DirectUnifiedDiscovery,
    Required,
    GloballyDisabled,
    ExternallyDisabled,
}

/// Observes installed Skills without changing the catalog or filesystem.
///
/// Root relationships are revalidated for every snapshot. A later write must
/// still repeat its own validation while holding the shared host lock.
pub fn inspect_installed_skills(
    catalog: &[SkillCatalogEntry],
    runtime: &SkillRuntime,
) -> Result<Vec<InstalledSkillSnapshot>, SkillReadError> {
    validate_distinct_roots(&runtime.source_root, &runtime.unified_root, &runtime.apps)?;
    validate_catalog_identity(catalog, runtime)?;
    let apps = runtime.apps().map(PreparedApp::new).collect::<Vec<_>>();
    let mut budget = SnapshotBudget::default();
    Ok(catalog
        .iter()
        .map(|entry| inspect_entry(entry, runtime, &apps, &mut budget))
        .collect())
}

pub(super) fn validate_catalog_identity(
    catalog: &[SkillCatalogEntry],
    runtime: &SkillRuntime,
) -> Result<(), SkillReadError> {
    if catalog.len() > MAX_SKILL_CATALOG_ENTRIES {
        return Err(SkillReadError::CatalogTooLarge {
            limit: MAX_SKILL_CATALOG_ENTRIES,
        });
    }
    let mut ids = HashSet::new();
    let mut directories = HashSet::new();
    let has_name_controls = runtime.apps.iter().any(|app| {
        builtin_app_registry()
            .for_app(&app.app)
            .skill_contract()
            .is_some_and(|contract| contract.config_target().is_some())
    });
    let case_insensitive_names = runtime.apps.iter().any(|app| {
        builtin_app_registry()
            .for_app(&app.app)
            .skill_contract()
            .and_then(|contract| contract.config_target())
            == Some(SkillConfigTarget::GeminiSettings)
    });
    let mut control_names = HashSet::new();
    for entry in catalog {
        if !ids.insert(entry.id()) {
            return Err(SkillReadError::DuplicateId {
                id: entry.id().to_owned(),
            });
        }
        let key = entry
            .directory()
            .nfc()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        if !directories.insert(key) {
            return Err(SkillReadError::DuplicateDirectory {
                directory: entry.directory().to_owned(),
            });
        }
        if has_name_controls {
            let key = if case_insensitive_names {
                entry.name().to_lowercase()
            } else {
                entry.name().to_owned()
            };
            if !control_names.insert(key) {
                return Err(SkillReadError::DuplicateControlName {
                    name: entry.name().to_owned(),
                });
            }
        }
    }
    Ok(())
}

struct PreparedApp<'a> {
    runtime: &'a SkillAppRuntime,
    controls: Option<Result<NativeSkillControls, ()>>,
}

impl<'a> PreparedApp<'a> {
    fn new(runtime: &'a SkillAppRuntime) -> Self {
        let controls = builtin_app_registry()
            .for_app(&runtime.app)
            .skill_contract()
            .and_then(|contract| contract.config_target())
            .map(|target| {
                let contents = runtime
                    .config
                    .as_ref()
                    .expect("runtime validates required config observations")
                    .contents();
                parse_native_controls(target, contents, runtime.hermes_platform.as_deref())
                    .map_err(|_| ())
            });
        Self { runtime, controls }
    }
}

fn inspect_entry(
    entry: &SkillCatalogEntry,
    runtime: &SkillRuntime,
    apps: &[PreparedApp<'_>],
    budget: &mut SnapshotBudget,
) -> InstalledSkillSnapshot {
    let source_path = runtime.source_root.join(entry.directory());
    let source = inspect_source(&source_path, budget);
    let apps = match source {
        SourceObservation::Ready(source) => {
            let reads_unified = apps.iter().any(|app| {
                builtin_app_registry()
                    .for_app(&app.runtime.app)
                    .skill_contract()
                    .is_some_and(|contract| {
                        contract.discovery() == SkillDiscovery::NativeAndUnified
                    })
            });
            let unified = if reads_unified {
                inspect_direct_relation(&source, &runtime.unified_root, entry.directory())
            } else {
                PathRelation::Missing
            };
            apps.iter()
                .map(|app| inspect_app(entry, app, &source, &unified))
                .collect()
        }
        SourceObservation::Missing => apps
            .iter()
            .map(|app| unavailable_state(entry, app.runtime, SkillControlReason::MissingSource))
            .collect(),
        SourceObservation::Invalid => apps
            .iter()
            .map(|app| unavailable_state(entry, app.runtime, SkillControlReason::InvalidSource))
            .collect(),
        SourceObservation::Unreadable => apps
            .iter()
            .map(|app| unavailable_state(entry, app.runtime, SkillControlReason::ObservationFailed))
            .collect(),
    };

    InstalledSkillSnapshot {
        id: entry.id().to_owned(),
        name: entry.name().to_owned(),
        description: entry.description().map(str::to_owned),
        directory: entry.directory().to_owned(),
        apps,
    }
}

fn unavailable_state(
    entry: &SkillCatalogEntry,
    runtime: &SkillAppRuntime,
    reason: SkillControlReason,
) -> SkillAppState {
    let selected = entry.selected_for(&runtime.app);
    SkillAppState {
        app: runtime.app.clone(),
        selected,
        enabled: None,
        writable: false,
        can_enable: false,
        can_disable: false,
        reason: Some(reason),
    }
}

fn inspect_app(
    entry: &SkillCatalogEntry,
    prepared: &PreparedApp<'_>,
    source: &ReadySource,
    unified: &PathRelation,
) -> SkillAppState {
    let runtime = prepared.runtime;
    let descriptor = builtin_app_registry().for_app(&runtime.app);
    let contract = descriptor
        .skill_contract()
        .expect("Skill runtime construction requires a contract");
    let selected = entry.selected_for(&runtime.app);
    let native = inspect_native_relation(entry, runtime, &source.path);
    if native.is_unreadable() {
        return state_unavailable(runtime, selected, SkillControlReason::ObservationFailed);
    }
    if native.is_external() {
        return state_unavailable(runtime, selected, SkillControlReason::NativeConflict);
    }

    let direct = if contract.discovery() == SkillDiscovery::NativeAndUnified {
        unified
    } else {
        &PathRelation::Missing
    };
    if direct.is_unreadable() {
        return state_unavailable(runtime, selected, SkillControlReason::ObservationFailed);
    }
    if direct.is_external() {
        return state_unavailable(runtime, selected, SkillControlReason::UnifiedConflict);
    }

    let present = native.is_selected() || direct.is_selected();
    let control = match prepared.controls.as_ref() {
        None => None,
        Some(Ok(controls)) => Some(controls.control_for(entry.name(), entry.directory())),
        Some(Err(())) => {
            return state_unavailable(runtime, selected, SkillControlReason::InvalidConfiguration)
        }
    };

    let enabled = match control {
        None | Some(NativeSkillControl::Enabled | NativeSkillControl::Required) => present,
        Some(
            NativeSkillControl::Disabled
            | NativeSkillControl::GloballyDisabled
            | NativeSkillControl::ExternallyDisabled,
        ) => false,
    };
    let reason = match control {
        Some(NativeSkillControl::Required) => Some(SkillControlReason::Required),
        Some(NativeSkillControl::GloballyDisabled) => Some(SkillControlReason::GloballyDisabled),
        Some(NativeSkillControl::ExternallyDisabled) => {
            Some(SkillControlReason::ExternallyDisabled)
        }
        None if direct.is_selected() => Some(SkillControlReason::DirectUnifiedDiscovery),
        _ => None,
    };
    let (can_enable, can_disable) = match reason {
        None => (true, true),
        Some(SkillControlReason::Required) => (true, false),
        Some(SkillControlReason::GloballyDisabled | SkillControlReason::ExternallyDisabled) => {
            (false, true)
        }
        Some(SkillControlReason::DirectUnifiedDiscovery) => (true, false),
        Some(_) => (false, false),
    };

    SkillAppState {
        app: runtime.app.clone(),
        selected,
        enabled: Some(enabled),
        writable: can_enable && can_disable,
        can_enable,
        can_disable,
        reason,
    }
}

fn state_unavailable(
    runtime: &SkillAppRuntime,
    selected: Option<bool>,
    reason: SkillControlReason,
) -> SkillAppState {
    SkillAppState {
        app: runtime.app.clone(),
        selected,
        enabled: None,
        writable: false,
        can_enable: false,
        can_disable: false,
        reason: Some(reason),
    }
}

enum SourceObservation {
    Ready(ReadySource),
    Missing,
    Invalid,
    Unreadable,
}

struct ReadySource {
    path: PathBuf,
    canonical: PathBuf,
}

fn inspect_source(path: &Path, budget: &mut SnapshotBudget) -> SourceObservation {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SourceObservation::Missing
        }
        Err(_) => return SourceObservation::Unreadable,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return SourceObservation::Invalid;
    }
    let manifest_path = path.join("SKILL.md");
    // Catalog metadata is host-supplied, and legacy manifests may omit YAML
    // frontmatter. Core validates the file boundary, not optional metadata.
    let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return SourceObservation::Invalid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SourceObservation::Invalid
        }
        Err(_) => return SourceObservation::Unreadable,
    };
    if budget.charge_entries(2).is_err() {
        return SourceObservation::Unreadable;
    }
    if manifest_metadata.len() > MAX_SKILL_MANIFEST_BYTES {
        return SourceObservation::Invalid;
    }
    if manifest_metadata.len() > budget.remaining_bytes() {
        budget.exhaust_bytes();
        return SourceObservation::Unreadable;
    }
    let read_limit = MAX_SKILL_MANIFEST_BYTES.min(budget.remaining_bytes());
    let mut manifest = match File::open(&manifest_path) {
        Ok(file) => file.take(read_limit.saturating_add(1)),
        Err(_) => return SourceObservation::Unreadable,
    };
    let mut contents = Vec::with_capacity(
        usize::try_from(manifest_metadata.len())
            .unwrap_or(0)
            .min(MAX_SKILL_MANIFEST_BYTES as usize),
    );
    if manifest.read_to_end(&mut contents).is_err() {
        let _ = budget.charge_bytes(contents.len() as u64);
        return SourceObservation::Unreadable;
    }
    let content_bytes = contents.len() as u64;
    if budget.charge_bytes(content_bytes).is_err() {
        return SourceObservation::Unreadable;
    }
    if content_bytes > MAX_SKILL_MANIFEST_BYTES || std::str::from_utf8(&contents).is_err() {
        return SourceObservation::Invalid;
    }
    match fs::canonicalize(path) {
        Ok(canonical) => SourceObservation::Ready(ReadySource {
            path: path.to_owned(),
            canonical,
        }),
        Err(_) => SourceObservation::Unreadable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathRelation {
    Missing,
    Selected,
    External,
    Blocked,
    Unreadable,
}

impl PathRelation {
    fn is_selected(self) -> bool {
        self == Self::Selected
    }

    fn is_external(self) -> bool {
        matches!(self, Self::External | Self::Blocked)
    }

    fn is_unreadable(self) -> bool {
        self == Self::Unreadable
    }
}

fn inspect_direct_relation(source: &ReadySource, root: &Path, directory: &str) -> PathRelation {
    let destination = root.join(directory);
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PathRelation::Missing,
        Err(_) => return PathRelation::Unreadable,
    };

    if !metadata.file_type().is_symlink() && !metadata.is_dir() {
        return PathRelation::Blocked;
    }
    match fs::canonicalize(&destination) {
        Ok(path) if path == source.canonical => PathRelation::Selected,
        Ok(path) => visibility_of_directory(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathRelation::Blocked,
        Err(_) => PathRelation::Unreadable,
    }
}

fn inspect_native_relation(
    entry: &SkillCatalogEntry,
    runtime: &SkillAppRuntime,
    source: &Path,
) -> PathRelation {
    match inspect_skill_reference(
        &runtime.state_root,
        &runtime.app,
        entry.id(),
        entry.directory(),
        source,
        &runtime.native_root,
    ) {
        SkillReferenceObservation::Missing | SkillReferenceObservation::ManagedMissing => {
            PathRelation::Missing
        }
        SkillReferenceObservation::ManagedPresent => PathRelation::Selected,
        SkillReferenceObservation::Unmanaged => PathRelation::External,
        SkillReferenceObservation::Conflict => PathRelation::Blocked,
        SkillReferenceObservation::Unreadable => PathRelation::Unreadable,
    }
}

fn visibility_of_directory(path: &Path) -> PathRelation {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => match fs::metadata(path.join("SKILL.md")) {
            Ok(manifest) if manifest.is_file() => PathRelation::External,
            Ok(_) => PathRelation::Blocked,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathRelation::Blocked,
            Err(_) => PathRelation::Unreadable,
        },
        Ok(_) => PathRelation::Blocked,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathRelation::Blocked,
        Err(_) => PathRelation::Unreadable,
    }
}

#[derive(Default)]
struct SnapshotBudget {
    entries: usize,
    bytes: u64,
}

impl SnapshotBudget {
    fn charge_entries(&mut self, count: usize) -> Result<(), ()> {
        let Some(next) = self.entries.checked_add(count) else {
            self.entries = MAX_SKILL_SNAPSHOT_ENTRIES;
            return Err(());
        };
        if next > MAX_SKILL_SNAPSHOT_ENTRIES {
            self.entries = MAX_SKILL_SNAPSHOT_ENTRIES;
            return Err(());
        }
        self.entries = next;
        Ok(())
    }

    fn charge_bytes(&mut self, count: u64) -> Result<(), ()> {
        let Some(next) = self.bytes.checked_add(count) else {
            self.exhaust_bytes();
            return Err(());
        };
        if next > MAX_SKILL_SNAPSHOT_BYTES {
            self.exhaust_bytes();
            return Err(());
        }
        self.bytes = next;
        Ok(())
    }

    fn remaining_bytes(&self) -> u64 {
        MAX_SKILL_SNAPSHOT_BYTES.saturating_sub(self.bytes)
    }

    fn exhaust_bytes(&mut self) {
        self.bytes = MAX_SKILL_SNAPSHOT_BYTES;
    }
}

/// Invalid host-resolved runtime inputs.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SkillRuntimeError {
    #[error("Skill root must be absolute: {path:?}")]
    RelativeRoot { path: PathBuf },
    #[error("application '{app}' does not support Skills")]
    UnsupportedApp { app: String },
    #[error("application '{app}' does not declare config target {target:?}")]
    UnexpectedConfig {
        app: String,
        target: crate::LogicalTarget,
    },
    #[error("application '{app}' requires config target {target:?}")]
    MissingConfig {
        app: String,
        target: SkillConfigTarget,
    },
    #[error("application '{app}' supplied {actual:?}, expected {expected:?}")]
    WrongConfig {
        app: String,
        expected: crate::LogicalTarget,
        actual: crate::LogicalTarget,
    },
    #[error("Skill config target {target:?} was not observed")]
    UnobservedConfig { target: crate::LogicalTarget },
    #[error("Skill config target {target:?} exceeds the {limit}-byte limit")]
    ConfigTooLarge {
        target: crate::LogicalTarget,
        limit: usize,
    },
    #[error("application '{app}' has duplicate Skill runtime inputs")]
    DuplicateApp { app: String },
    #[error("application '{app}' cannot use a Hermes platform context")]
    UnexpectedHermesPlatform { app: String },
    #[error("Hermes platform context is invalid")]
    InvalidHermesPlatform,
    #[error("at least one Skill application runtime is required")]
    NoApps,
    #[error("Skill roots overlap: {left:?}, {right:?}")]
    OverlappingRoots { left: PathBuf, right: PathBuf },
    #[error("Skill root could not be resolved at {path:?}: {message}")]
    RootResolution { path: PathBuf, message: String },
}

/// Invalid identity relationships in a host-supplied catalog snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SkillReadError {
    #[error("Skill catalog exceeds the {limit}-entry limit")]
    CatalogTooLarge { limit: usize },
    #[error(transparent)]
    Runtime(#[from] SkillRuntimeError),
    #[error("duplicate Skill id: {id:?}")]
    DuplicateId { id: String },
    #[error("duplicate Skill directory: {directory:?}")]
    DuplicateDirectory { directory: String },
    #[error("duplicate native Skill control name: {name:?}")]
    DuplicateControlName { name: String },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{tempdir, TempDir};

    use super::*;
    use crate::SkillCatalogColumn;

    fn catalog_entry(directory: &str, selected: bool) -> SkillCatalogEntry {
        let selections = crate::skill_catalog_columns().map(|column| (column, selected));
        SkillCatalogEntry::try_new(
            format!("owner/repo:{directory}"),
            directory,
            Some("test Skill".to_owned()),
            directory,
            selections,
        )
        .expect("catalog entry")
    }

    fn write_skill(root: &Path, directory: &str, body: &str) {
        let path = root.join(directory);
        fs::create_dir_all(&path).expect("create Skill directory");
        fs::write(path.join("SKILL.md"), body).expect("write Skill manifest");
    }

    fn app_runtime(root: &Path, app: AppType) -> SkillAppRuntime {
        let config = builtin_app_registry()
            .for_app(&app)
            .skill_contract()
            .and_then(|contract| contract.config_target())
            .map(|target| ObservedDocument::missing(target.logical_target()));
        let state = root.parent().unwrap_or(root).join(format!(
            ".{}-{}-skill-state",
            root.file_name().unwrap_or_default().to_string_lossy(),
            app.as_str()
        ));
        SkillAppRuntime::try_new(app, root, state, config).expect("app runtime")
    }

    fn skill_runtime(
        temp: &TempDir,
        source: impl Into<PathBuf>,
        unified: impl Into<PathBuf>,
        apps: impl IntoIterator<Item = SkillAppRuntime>,
    ) -> Result<SkillRuntime, SkillRuntimeError> {
        let _ = temp;
        SkillRuntime::try_new(source, unified, apps)
    }

    fn state<'a>(snapshot: &'a InstalledSkillSnapshot, app: &AppType) -> &'a SkillAppState {
        snapshot
            .apps()
            .find(|state| state.app() == app)
            .expect("app state")
    }

    #[test]
    fn selected_catalog_does_not_claim_an_unowned_plain_copy() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "source");
        write_skill(&native, "demo", "source");

        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");
        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);

        assert_eq!(claude.selected(), Some(true));
        assert_eq!(claude.enabled(), None);
        assert!(!claude.writable());
        assert_eq!(claude.reason(), Some(SkillControlReason::NativeConflict));
    }

    #[test]
    fn an_unselected_plain_copy_is_not_claimed() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "same");
        write_skill(&native, "demo", "same");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);
        assert_eq!(claude.enabled(), None);
        assert!(!claude.writable());
        assert_eq!(claude.reason(), Some(SkillControlReason::NativeConflict));
    }

    #[test]
    fn pi_does_not_claim_an_unmarked_plain_copy() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "same");
        write_skill(&native, "demo", "same");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Pi)],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshots");
        let pi = state(&snapshots[0], &AppType::Pi);
        assert_eq!(pi.selected(), Some(false));
        assert_eq!(pi.enabled(), None);
        assert!(!pi.writable());
        assert_eq!(pi.reason(), Some(SkillControlReason::NativeConflict));
    }

    #[test]
    fn pi_selection_remains_catalog_backed_when_the_native_entry_is_invalid() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "source");
        fs::create_dir_all(native.join("demo")).expect("native directory");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Pi)],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshots");
        let pi = state(&snapshots[0], &AppType::Pi);
        assert_eq!(pi.selected(), Some(false));
        assert_eq!(pi.enabled(), None);
        assert_eq!(pi.reason(), Some(SkillControlReason::NativeConflict));
    }

    #[test]
    fn direct_unified_discovery_is_visible_but_not_disableable_without_a_control() {
        let temp = tempdir().expect("tempdir");
        let unified = temp.path().join("unified");
        let codex_native = temp.path().join("codex-native");
        let pi_native = temp.path().join("pi-native");
        write_skill(&unified, "demo", "source");
        let runtime = skill_runtime(
            &temp,
            &unified,
            &unified,
            [
                app_runtime(&codex_native, AppType::Codex),
                app_runtime(&pi_native, AppType::Pi),
            ],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshots");
        for app in [AppType::Codex, AppType::Pi] {
            let app_state = state(&snapshots[0], &app);
            assert_eq!(app_state.enabled(), Some(true));
            assert!(!app_state.writable());
            assert_eq!(
                app_state.reason(),
                Some(SkillControlReason::DirectUnifiedDiscovery)
            );
        }
        assert_eq!(state(&snapshots[0], &AppType::Pi).selected(), Some(false));
    }

    #[test]
    fn native_controls_resolve_effective_state() {
        let temp = tempdir().expect("tempdir");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        write_skill(&unified, "demo", "source");
        let config = ObservedDocument::present(
            SkillConfigTarget::GeminiSettings.logical_target(),
            br#"{ skills: { disabled: ["demo"] } }"#.to_vec(),
        );
        let gemini = SkillAppRuntime::try_new(
            AppType::Gemini,
            &native,
            temp.path().join("gemini-state"),
            Some(config),
        )
        .expect("Gemini runtime");
        let runtime = skill_runtime(&temp, &unified, &unified, [gemini]).expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let gemini = state(&snapshots[0], &AppType::Gemini);
        assert_eq!(gemini.enabled(), Some(false));
        assert!(gemini.writable());
        assert_eq!(gemini.reason(), None);
    }

    #[test]
    fn missing_sources_are_read_only_without_touching_native_paths() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("missing-source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);
        assert_eq!(claude.enabled(), None);
        assert!(!claude.writable());
        assert_eq!(claude.reason(), Some(SkillControlReason::MissingSource));
        assert!(!native.exists());
        assert!(!unified.exists());
    }

    #[test]
    fn pi_catalog_selection_is_kept_when_the_source_is_missing() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("missing-source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        fs::create_dir_all(native.join("present")).expect("native Skill directory");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Pi)],
        )
        .expect("runtime");
        let catalog = [
            catalog_entry("present", false),
            catalog_entry("missing", false),
        ];

        let snapshots = inspect_installed_skills(&catalog, &runtime).expect("snapshots");
        assert_eq!(state(&snapshots[0], &AppType::Pi).selected(), Some(false));
        assert_eq!(state(&snapshots[1], &AppType::Pi).selected(), Some(false));
        for snapshot in &snapshots {
            let pi = state(snapshot, &AppType::Pi);
            assert_eq!(pi.enabled(), None);
            assert!(!pi.writable());
            assert_eq!(pi.reason(), Some(SkillControlReason::MissingSource));
        }
    }

    #[test]
    fn required_hermes_skill_stays_read_only_when_not_installed() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "hermes-agent", "source");
        let config = ObservedDocument::present(
            SkillConfigTarget::HermesConfig.logical_target(),
            b"skills:\n  platform_disabled:\n    telegram: [hermes-agent]\n".to_vec(),
        );
        let hermes = SkillAppRuntime::try_new(
            AppType::Hermes,
            &native,
            temp.path().join("hermes-state"),
            Some(config),
        )
        .expect("Hermes runtime")
        .try_with_hermes_platform("telegram")
        .expect("Hermes platform");
        let runtime = skill_runtime(&temp, &source, &unified, [hermes]).expect("runtime");

        let selections = crate::skill_catalog_columns().map(|column| (column, false));
        let required = SkillCatalogEntry::try_new(
            "nous/hermes:hermes-agent",
            "Hermes Agent",
            None,
            "hermes-agent",
            selections,
        )
        .expect("required Skill");
        let snapshots = inspect_installed_skills(&[required], &runtime).expect("snapshots");
        let hermes = state(&snapshots[0], &AppType::Hermes);
        assert_eq!(hermes.enabled(), Some(false));
        assert!(!hermes.writable());
        assert_eq!(hermes.reason(), Some(SkillControlReason::Required));
    }

    #[test]
    fn runtime_rejects_wrong_or_unobserved_config_documents() {
        let temp = tempdir().expect("tempdir");
        let wrong = ObservedDocument::missing(crate::LogicalTarget::GrokConfig);
        assert!(matches!(
            SkillAppRuntime::try_new(
                AppType::Gemini,
                temp.path(),
                temp.path().join("state"),
                Some(wrong),
            ),
            Err(SkillRuntimeError::WrongConfig { .. })
        ));
        let unobserved = ObservedDocument::unobserved(crate::LogicalTarget::GeminiSettings);
        assert!(matches!(
            SkillAppRuntime::try_new(
                AppType::Gemini,
                temp.path(),
                temp.path().join("state"),
                Some(unobserved),
            ),
            Err(SkillRuntimeError::UnobservedConfig { .. })
        ));
        let claude = app_runtime(temp.path(), AppType::Claude);
        assert!(matches!(
            claude.try_with_hermes_platform("telegram"),
            Err(SkillRuntimeError::UnexpectedHermesPlatform { .. })
        ));
    }

    #[test]
    fn runtime_rejects_roots_that_could_modify_the_shared_source() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let unified = temp.path().join("unified");
        let claude = app_runtime(&source, AppType::Claude);

        assert!(matches!(
            skill_runtime(&temp, &source, &unified, [claude]),
            Err(SkillRuntimeError::OverlappingRoots { .. })
        ));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn runtime_rejects_case_only_missing_root_aliases() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("Skills");
        let unified = temp.path().join("skills");
        let native = temp.path().join("native");

        assert!(matches!(
            skill_runtime(
                &temp,
                &source,
                &unified,
                [app_runtime(&native, AppType::Pi)]
            ),
            Err(SkillRuntimeError::OverlappingRoots { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_rejects_distinct_aliases_for_the_same_root() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let alias = temp.path().join("source-alias");
        let native = temp.path().join("native");
        fs::create_dir_all(&source).expect("source root");
        symlink(&source, &alias).expect("source alias");

        assert!(matches!(
            skill_runtime(&temp, &source, &alias, [app_runtime(&native, AppType::Pi)]),
            Err(SkillRuntimeError::OverlappingRoots { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn source_entries_cannot_escape_through_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let external = temp.path().join("external");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        fs::create_dir_all(&source).expect("source root");
        write_skill(&external, "demo", "external");
        symlink(external.join("demo"), source.join("demo")).expect("source entry link");
        fs::create_dir_all(&native).expect("native root");
        symlink(external.join("demo"), native.join("demo")).expect("native entry link");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);
        assert_eq!(claude.enabled(), None);
        assert!(!claude.writable());
        assert_eq!(claude.reason(), Some(SkillControlReason::InvalidSource));
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_revalidate_root_aliases_after_runtime_construction() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        write_skill(&source, "demo", "source");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");
        symlink(&source, &unified).expect("late unified alias");

        assert!(matches!(
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime),
            Err(SkillReadError::Runtime(
                SkillRuntimeError::OverlappingRoots { .. }
            ))
        ));
    }

    #[test]
    fn source_manifests_are_bounded_utf8_files() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        write_skill(&source, "binary", "source");
        write_skill(&source, "oversized", "source");
        fs::write(source.join("binary/SKILL.md"), [0xff]).expect("binary manifest");
        fs::write(
            source.join("oversized/SKILL.md"),
            vec![b'x'; MAX_SKILL_MANIFEST_BYTES as usize + 1],
        )
        .expect("oversized manifest");
        let runtime = skill_runtime(
            &temp,
            &source,
            &unified,
            [app_runtime(&native, AppType::Claude)],
        )
        .expect("runtime");
        let catalog = [
            catalog_entry("binary", true),
            catalog_entry("oversized", true),
        ];

        let snapshots = inspect_installed_skills(&catalog, &runtime).expect("snapshots");
        for snapshot in &snapshots {
            assert_eq!(
                state(snapshot, &AppType::Claude).reason(),
                Some(SkillControlReason::InvalidSource)
            );
        }
    }

    #[test]
    fn snapshots_bound_the_catalog_size() {
        let temp = tempdir().expect("tempdir");
        let runtime = skill_runtime(
            &temp,
            temp.path().join("source"),
            temp.path().join("unified"),
            [app_runtime(&temp.path().join("native"), AppType::Claude)],
        )
        .expect("runtime");
        let catalog = vec![catalog_entry("demo", false); MAX_SKILL_CATALOG_ENTRIES + 1];

        assert!(matches!(
            inspect_installed_skills(&catalog, &runtime),
            Err(SkillReadError::CatalogTooLarge { .. })
        ));
    }

    #[test]
    fn name_based_native_controls_reject_ambiguous_catalog_entries() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        let gemini = app_runtime(&native, AppType::Gemini);
        let runtime = skill_runtime(&temp, &source, &unified, [gemini]).expect("runtime");
        let make_entry = |id: &str, directory: &str| {
            SkillCatalogEntry::try_new(
                id,
                "same-name",
                None,
                directory,
                crate::skill_catalog_columns().map(|column| (column, false)),
            )
            .unwrap()
        };
        let catalog = [make_entry("one", "one"), make_entry("two", "two")];

        assert!(matches!(
            inspect_installed_skills(&catalog, &runtime),
            Err(SkillReadError::DuplicateControlName { .. })
        ));
    }

    #[test]
    fn catalog_rejects_unicode_equivalent_directories() {
        let temp = tempdir().expect("tempdir");
        let runtime = skill_runtime(
            &temp,
            temp.path().join("source"),
            temp.path().join("unified"),
            [app_runtime(&temp.path().join("native"), AppType::Claude)],
        )
        .expect("runtime");
        let entry = |id: &str, directory: &str| {
            SkillCatalogEntry::try_new(
                id,
                id,
                None,
                directory,
                crate::skill_catalog_columns().map(|column| (column, false)),
            )
            .expect("entry")
        };
        let catalog = [entry("one", "é"), entry("two", "e\u{301}")];

        assert!(matches!(
            inspect_installed_skills(&catalog, &runtime),
            Err(SkillReadError::DuplicateDirectory { .. })
        ));
    }

    #[test]
    fn snapshots_bound_manifest_work() {
        let temp = tempdir().expect("tempdir");
        write_skill(temp.path(), "demo", "manifest");
        let skill = temp.path().join("demo");
        let mut budget = SnapshotBudget {
            bytes: MAX_SKILL_SNAPSHOT_BYTES,
            ..SnapshotBudget::default()
        };

        assert!(matches!(
            inspect_source(&skill, &mut budget),
            SourceObservation::Unreadable
        ));
    }

    #[test]
    fn snapshot_wire_state_keeps_selection_and_visibility_distinct() {
        let temp = tempdir().expect("tempdir");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        write_skill(&unified, "demo", "source");
        let runtime = skill_runtime(
            &temp,
            &unified,
            &unified,
            [app_runtime(&native, AppType::Pi)],
        )
        .expect("runtime");
        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshot");

        assert_eq!(
            serde_json::to_value(state(&snapshots[0], &AppType::Pi)).expect("serialize state"),
            serde_json::json!({
                "app": "pi",
                "selected": false,
                "enabled": true,
                "writable": false,
                "canEnable": true,
                "canDisable": false,
                "reason": "directUnifiedDiscovery",
            })
        );
    }

    #[test]
    fn runtime_debug_redacts_native_config_contents() {
        let temp = tempdir().expect("tempdir");
        let secret = "do-not-log-this";
        let config = ObservedDocument::present(
            crate::LogicalTarget::GeminiSettings,
            format!("{{ secret: '{secret}' }}"),
        );
        let runtime = SkillAppRuntime::try_new(
            AppType::Gemini,
            temp.path(),
            temp.path().join("state"),
            Some(config),
        )
        .expect("runtime");
        assert!(!format!("{runtime:?}").contains(secret));
    }

    #[test]
    fn pi_selection_comes_from_the_catalog() {
        let contract = builtin_app_registry()
            .for_app(&AppType::Pi)
            .skill_contract()
            .expect("Pi Skill contract");
        assert_eq!(contract.catalog_column().as_str(), "enabled_pi");
        assert_eq!(
            catalog_entry("demo", false).selected_for(&AppType::Pi),
            Some(false)
        );
    }

    #[test]
    fn sealed_catalog_columns_remain_usable_by_host_rows() {
        let columns = catalog_entry("demo", false)
            .selections()
            .map(|(column, _)| column)
            .collect::<Vec<SkillCatalogColumn>>();
        assert_eq!(columns.len(), 7);
    }
}
