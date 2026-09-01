use std::{
    collections::{HashMap, HashSet},
    fmt,
    fs::{self, DirEntry, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    builtin_app_registry, AppType, ObservedDocument, SkillConfigTarget, SkillDiscovery,
    SkillSelectionStore, MAX_OPERATION_CONTENT_BYTES,
};

use super::{
    config::{parse_native_controls, NativeSkillControl, NativeSkillControls},
    SkillCatalogEntry,
};

const MAX_SKILL_TREE_ENTRIES: usize = 10_000;
const MAX_SKILL_TREE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_HERMES_PLATFORM_BYTES: usize = 128;

/// Host-resolved runtime inputs for one application.
///
/// Path overrides and platform-specific resolution remain host-owned. Core
/// owns the common observation and state policy once those paths are supplied.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillAppRuntime {
    app: AppType,
    native_root: PathBuf,
    config: Option<ObservedDocument>,
    hermes_platform: Option<String>,
}

impl SkillAppRuntime {
    pub fn try_new(
        app: AppType,
        native_root: impl Into<PathBuf>,
        config: Option<ObservedDocument>,
    ) -> Result<Self, SkillRuntimeError> {
        let native_root = native_root.into();
        if !native_root.is_absolute() {
            return Err(SkillRuntimeError::RelativeRoot { path: native_root });
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

    /// Returns the active Hermes gateway platform, when the host supplied it.
    pub fn hermes_platform(&self) -> Option<&str> {
        self.hermes_platform.as_deref()
    }
}

impl fmt::Debug for SkillAppRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillAppRuntime")
            .field("app", &self.app)
            .field("native_root", &self.native_root)
            .field("config", &self.config)
            .field("hermes_platform", &self.hermes_platform)
            .finish()
    }
}

/// Complete read-only runtime context for a requested set of applications.
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

    let mut resolved_apps: Vec<(&Path, PathBuf)> = Vec::with_capacity(apps.len());
    for app in apps {
        let resolved = resolve_root(&app.native_root)?;
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
                    left: app.native_root.clone(),
                    right: other_path.to_owned(),
                });
            }
        }
        resolved_apps.push((&app.native_root, resolved));
    }
    Ok(())
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn resolve_root(path: &Path) -> Result<PathBuf, SkillRuntimeError> {
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
    reason: Option<SkillControlReason>,
}

impl SkillAppState {
    pub fn app(&self) -> &AppType {
        &self.app
    }

    /// Returns the persisted request, or `None` when native selection could not
    /// be observed.
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
pub fn inspect_installed_skills(
    catalog: &[SkillCatalogEntry],
    runtime: &SkillRuntime,
) -> Result<Vec<InstalledSkillSnapshot>, SkillReadError> {
    validate_catalog_identity(catalog)?;
    let apps = runtime.apps().map(PreparedApp::new).collect::<Vec<_>>();
    Ok(catalog
        .iter()
        .map(|entry| inspect_entry(entry, runtime, &apps))
        .collect())
}

fn validate_catalog_identity(catalog: &[SkillCatalogEntry]) -> Result<(), SkillReadError> {
    let mut ids = HashSet::new();
    let mut directories = HashSet::new();
    for entry in catalog {
        if !ids.insert(entry.id()) {
            return Err(SkillReadError::DuplicateId {
                id: entry.id().to_owned(),
            });
        }
        let key = entry.directory().to_lowercase();
        if !directories.insert(key) {
            return Err(SkillReadError::DuplicateDirectory {
                directory: entry.directory().to_owned(),
            });
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
) -> InstalledSkillSnapshot {
    let source_path = runtime.source_root.join(entry.directory());
    let source = inspect_source(&source_path);
    let apps = match source {
        SourceObservation::Ready(mut source) => {
            let reads_unified = apps.iter().any(|app| {
                builtin_app_registry()
                    .for_app(&app.runtime.app)
                    .skill_contract()
                    .is_some_and(|contract| {
                        contract.discovery() == SkillDiscovery::NativeAndUnified
                    })
            });
            let unified = if reads_unified {
                inspect_relation(
                    &mut source,
                    &runtime.unified_root,
                    entry.directory(),
                    false,
                    true,
                )
            } else {
                PathRelation::Missing
            };
            apps.iter()
                .map(|app| inspect_app(entry, app, &mut source, &unified))
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
    let contract = builtin_app_registry()
        .for_app(&runtime.app)
        .skill_contract()
        .expect("Skill runtime construction requires a contract");
    let selected = match contract.selection_store() {
        SkillSelectionStore::CatalogColumn(_) => entry.selected_for(&runtime.app),
        SkillSelectionStore::NativeDirectory => {
            observe_native_directory_selection(&runtime.native_root, entry.directory())
        }
    };
    SkillAppState {
        app: runtime.app.clone(),
        selected,
        enabled: None,
        writable: false,
        reason: Some(reason),
    }
}

fn inspect_app(
    entry: &SkillCatalogEntry,
    prepared: &PreparedApp<'_>,
    source: &mut ReadySource,
    unified: &PathRelation,
) -> SkillAppState {
    let runtime = prepared.runtime;
    let descriptor = builtin_app_registry().for_app(&runtime.app);
    let contract = descriptor
        .skill_contract()
        .expect("Skill runtime construction requires a contract");
    let catalog_selected = entry.selected_for(&runtime.app);
    let allow_matching_copy = catalog_selected == Some(true);
    let native = inspect_relation(
        source,
        &runtime.native_root,
        entry.directory(),
        allow_matching_copy,
        false,
    );
    let selected = match contract.selection_store() {
        SkillSelectionStore::CatalogColumn(_) => catalog_selected,
        SkillSelectionStore::NativeDirectory => native.presence(),
    };
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
        Some(Ok(controls)) => Some(controls.control_for(entry.name())),
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

    SkillAppState {
        app: runtime.app.clone(),
        selected,
        enabled: Some(enabled),
        writable: reason.is_none(),
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
    digest: Option<[u8; 32]>,
}

fn inspect_source(path: &Path) -> SourceObservation {
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
    match fs::symlink_metadata(path.join("SKILL.md")) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return SourceObservation::Invalid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SourceObservation::Invalid
        }
        Err(_) => return SourceObservation::Unreadable,
    }
    match fs::canonicalize(path) {
        Ok(canonical) => SourceObservation::Ready(ReadySource {
            path: path.to_owned(),
            canonical,
            digest: None,
        }),
        Err(_) => SourceObservation::Unreadable,
    }
}

fn observe_native_directory_selection(root: &Path, directory: &str) -> Option<bool> {
    let path = root.join(directory);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::metadata(path) {
            Ok(target) => Some(target.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        },
        Ok(metadata) => Some(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
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

    fn presence(self) -> Option<bool> {
        match self {
            Self::Missing | Self::Blocked => Some(false),
            Self::Selected | Self::External => Some(true),
            Self::Unreadable => None,
        }
    }
}

fn inspect_relation(
    source: &mut ReadySource,
    root: &Path,
    directory: &str,
    allow_matching_copy: bool,
    allow_direct_source: bool,
) -> PathRelation {
    let destination = root.join(directory);
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PathRelation::Missing,
        Err(_) => return PathRelation::Unreadable,
    };

    if metadata.file_type().is_symlink() {
        return match fs::canonicalize(&destination) {
            Ok(target) if target == source.canonical => PathRelation::Selected,
            Ok(target) => visibility_of_directory(&target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathRelation::Blocked,
            Err(_) => PathRelation::Unreadable,
        };
    }
    if !metadata.is_dir() {
        return PathRelation::Blocked;
    }
    match fs::canonicalize(&destination) {
        Ok(path) if path == source.canonical => {
            return if allow_direct_source {
                PathRelation::Selected
            } else {
                PathRelation::External
            }
        }
        Ok(_) => {}
        Err(_) => return PathRelation::Unreadable,
    }
    if allow_matching_copy {
        let source_digest = match source.digest {
            Some(digest) => digest,
            None => match tree_digest(&source.path) {
                Ok(digest) => {
                    source.digest = Some(digest);
                    digest
                }
                Err(_) => return PathRelation::Unreadable,
            },
        };
        match tree_digest(&destination) {
            Ok(destination_digest) if destination_digest == source_digest => {
                return PathRelation::Selected
            }
            Ok(_) => {}
            Err(_) => return PathRelation::Unreadable,
        }
    }
    visibility_of_directory(&destination)
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
struct TreeBudget {
    entries: usize,
    bytes: u64,
}

fn tree_digest(root: &Path) -> Result<[u8; 32], ()> {
    let mut budget = TreeBudget::default();
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-skill-read-v1\0");
    hash_directory(root, root, &mut budget, &mut hasher)?;
    Ok(hasher.finalize().into())
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    budget: &mut TreeBudget,
    hasher: &mut Sha256,
) -> Result<(), ()> {
    let mut entries = read_directory_entries(directory, budget)?;
    entries.sort_by_key(DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|_| ())?;
        let relative = relative.to_str().ok_or(())?.replace('\\', "/");
        let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
        if metadata.file_type().is_dir() {
            hasher.update(b"d\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hash_directory(root, &path, budget, hasher)?;
        } else if metadata.file_type().is_file() {
            hasher.update(b"f\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hash_file(&path, &metadata, budget, hasher)?;
        } else {
            return Err(());
        }
    }
    Ok(())
}

fn read_directory_entries(directory: &Path, budget: &mut TreeBudget) -> Result<Vec<DirEntry>, ()> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).map_err(|_| ())? {
        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > MAX_SKILL_TREE_ENTRIES {
            return Err(());
        }
        entries.push(entry.map_err(|_| ())?);
    }
    Ok(entries)
}

fn hash_file(
    path: &Path,
    before: &fs::Metadata,
    budget: &mut TreeBudget,
    hasher: &mut Sha256,
) -> Result<(), ()> {
    let mut file = File::open(path).map_err(|_| ())?;
    hasher.update(before.len().to_le_bytes());
    let mut read_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            break;
        }
        let count_u64 = u64::try_from(count).map_err(|_| ())?;
        budget.bytes = budget.bytes.saturating_add(count_u64);
        if budget.bytes > MAX_SKILL_TREE_BYTES {
            return Err(());
        }
        read_bytes = read_bytes.saturating_add(count_u64);
        hasher.update(&buffer[..count]);
    }
    let after = fs::symlink_metadata(path).map_err(|_| ())?;
    if !after.file_type().is_file() || before.len() != read_bytes || after.len() != read_bytes {
        return Err(());
    }
    hasher.update(b"\0");
    Ok(())
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
    #[error("duplicate Skill id: {id:?}")]
    DuplicateId { id: String },
    #[error("duplicate Skill directory: {directory:?}")]
    DuplicateDirectory { directory: String },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::{SkillCatalogColumn, SkillSelectionStore};

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
        SkillAppRuntime::try_new(app, root, config).expect("app runtime")
    }

    fn state<'a>(snapshot: &'a InstalledSkillSnapshot, app: &AppType) -> &'a SkillAppState {
        snapshot
            .apps()
            .find(|state| state.app() == app)
            .expect("app state")
    }

    #[test]
    fn catalog_selection_and_native_state_are_reported_separately() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "source");
        write_skill(&native, "demo", "source");

        let runtime =
            SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Claude)])
                .expect("runtime");
        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);

        assert_eq!(claude.selected(), Some(true));
        assert_eq!(claude.enabled(), Some(true));
        assert!(claude.writable());
        assert_eq!(claude.reason(), None);
    }

    #[test]
    fn an_unselected_plain_copy_is_not_claimed() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        write_skill(&source, "demo", "same");
        write_skill(&native, "demo", "same");
        let runtime =
            SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Claude)])
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
        let runtime = SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Pi)])
            .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", false)], &runtime).expect("snapshots");
        let pi = state(&snapshots[0], &AppType::Pi);
        assert_eq!(pi.selected(), Some(true));
        assert_eq!(pi.enabled(), None);
        assert!(!pi.writable());
        assert_eq!(pi.reason(), Some(SkillControlReason::NativeConflict));
    }

    #[test]
    fn direct_unified_discovery_is_visible_but_not_disableable_without_a_control() {
        let temp = tempdir().expect("tempdir");
        let unified = temp.path().join("unified");
        let codex_native = temp.path().join("codex-native");
        let pi_native = temp.path().join("pi-native");
        write_skill(&unified, "demo", "source");
        let runtime = SkillRuntime::try_new(
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
        let gemini = SkillAppRuntime::try_new(AppType::Gemini, &native, Some(config))
            .expect("Gemini runtime");
        let runtime = SkillRuntime::try_new(&unified, &unified, [gemini]).expect("runtime");

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
        let runtime =
            SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Claude)])
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
    fn pi_selection_is_observed_when_the_source_is_missing() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("missing-source");
        let native = temp.path().join("native");
        let unified = temp.path().join("unified");
        fs::create_dir_all(native.join("present")).expect("native Skill directory");
        let runtime = SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Pi)])
            .expect("runtime");
        let catalog = [
            catalog_entry("present", false),
            catalog_entry("missing", false),
        ];

        let snapshots = inspect_installed_skills(&catalog, &runtime).expect("snapshots");
        assert_eq!(state(&snapshots[0], &AppType::Pi).selected(), Some(true));
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
        let hermes = SkillAppRuntime::try_new(AppType::Hermes, &native, Some(config))
            .expect("Hermes runtime")
            .try_with_hermes_platform("telegram")
            .expect("Hermes platform");
        let runtime = SkillRuntime::try_new(&source, &unified, [hermes]).expect("runtime");

        let snapshots = inspect_installed_skills(&[catalog_entry("hermes-agent", false)], &runtime)
            .expect("snapshots");
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
            SkillAppRuntime::try_new(AppType::Gemini, temp.path(), Some(wrong)),
            Err(SkillRuntimeError::WrongConfig { .. })
        ));
        let unobserved = ObservedDocument::unobserved(crate::LogicalTarget::GeminiSettings);
        assert!(matches!(
            SkillAppRuntime::try_new(AppType::Gemini, temp.path(), Some(unobserved)),
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
            SkillRuntime::try_new(&source, &unified, [claude]),
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
            SkillRuntime::try_new(&source, &alias, [app_runtime(&native, AppType::Pi)]),
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
        let runtime =
            SkillRuntime::try_new(&source, &unified, [app_runtime(&native, AppType::Claude)])
                .expect("runtime");

        let snapshots =
            inspect_installed_skills(&[catalog_entry("demo", true)], &runtime).expect("snapshots");
        let claude = state(&snapshots[0], &AppType::Claude);
        assert_eq!(claude.enabled(), None);
        assert!(!claude.writable());
        assert_eq!(claude.reason(), Some(SkillControlReason::InvalidSource));
    }

    #[test]
    fn snapshot_wire_state_keeps_selection_and_visibility_distinct() {
        let temp = tempdir().expect("tempdir");
        let unified = temp.path().join("unified");
        let native = temp.path().join("native");
        write_skill(&unified, "demo", "source");
        let runtime =
            SkillRuntime::try_new(&unified, &unified, [app_runtime(&native, AppType::Pi)])
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
        let runtime =
            SkillAppRuntime::try_new(AppType::Gemini, temp.path(), Some(config)).expect("runtime");
        assert!(!format!("{runtime:?}").contains(secret));
    }

    #[test]
    fn native_directory_contract_is_not_catalog_backed() {
        let contract = builtin_app_registry()
            .for_app(&AppType::Pi)
            .skill_contract()
            .expect("Pi Skill contract");
        assert_eq!(
            contract.selection_store(),
            SkillSelectionStore::NativeDirectory
        );
        assert_eq!(
            catalog_entry("demo", false).selected_for(&AppType::Pi),
            None
        );
    }

    #[test]
    fn sealed_catalog_columns_remain_usable_by_host_rows() {
        let columns = catalog_entry("demo", false)
            .selections()
            .map(|(column, _)| column)
            .collect::<Vec<SkillCatalogColumn>>();
        assert_eq!(columns.len(), 6);
    }
}
