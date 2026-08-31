//! Shared contracts and guarded filesystem materialization for installed Skills.

use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Maximum number of entries accepted in one installed Skill tree.
pub const MAX_SKILL_TREE_ENTRIES: usize = 10_000;
/// Maximum total file content accepted in one installed Skill tree.
pub const MAX_SKILL_TREE_BYTES: u64 = 512 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Where the authoritative enabled state for an application is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillActivationSource {
    /// The shared catalog contains an `enabled_*` flag.
    CatalogFlag,
    /// Presence in the native Skill directory is authoritative.
    NativePresence,
}

/// Product-neutral Skill behavior declared by an application descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillAppContract {
    activation_source: SkillActivationSource,
    catalog_column: Option<&'static str>,
}

impl SkillAppContract {
    /// Declares a catalog-backed application and its stable shared column.
    pub const fn catalog_flag(column: &'static str) -> Self {
        Self {
            activation_source: SkillActivationSource::CatalogFlag,
            catalog_column: Some(column),
        }
    }

    /// Declares an application whose native directory is authoritative.
    pub const fn native_presence() -> Self {
        Self {
            activation_source: SkillActivationSource::NativePresence,
            catalog_column: None,
        }
    }

    /// Returns the authoritative source for this application's enabled state.
    pub const fn activation_source(self) -> SkillActivationSource {
        self.activation_source
    }

    /// Returns the shared catalog column when activation is catalog-backed.
    pub const fn catalog_column(self) -> Option<&'static str> {
        self.catalog_column
    }
}

pub const CLAUDE_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_claude");
pub const CODEX_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_codex");
pub const GEMINI_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_gemini");
pub const GROKBUILD_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_grokbuild");
pub const OPENCODE_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_opencode");
pub const HERMES_SKILLS: SkillAppContract = SkillAppContract::catalog_flag("enabled_hermes");
pub const PI_SKILLS: SkillAppContract = SkillAppContract::native_presence();

/// How an installed Skill is materialized in an application's native directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSyncMethod {
    /// Prefer a symbolic link and fall back to a verified copy.
    #[default]
    Auto,
    /// Require a symbolic link.
    Symlink,
    /// Materialize a verified copy.
    Copy,
}

/// The verified state of one native Skill destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDeploymentState {
    Missing,
    Linked,
    Copied,
}

/// A filesystem failure produced while inspecting or changing a Skill deployment.
#[derive(Debug, Error)]
pub enum SkillConfigError {
    #[error("invalid Skill directory name: {directory:?}")]
    InvalidDirectory { directory: String },
    #[error("Skill roots must be absolute: {path:?}")]
    RelativeRoot { path: PathBuf },
    #[error("Skill source does not exist: {path:?}")]
    MissingSource { path: PathBuf },
    #[error("Skill source is missing SKILL.md: {path:?}")]
    MissingManifest { path: PathBuf },
    #[error("Skill source and destination roots overlap: {source_root:?}, {destination_root:?}")]
    OverlappingRoots {
        source_root: PathBuf,
        destination_root: PathBuf,
    },
    #[error("native Skill destination conflicts with the shared source: {path:?}")]
    Conflict { path: PathBuf },
    #[error("unsupported entry in Skill tree: {path:?}")]
    UnsupportedEntry { path: PathBuf },
    #[error("Skill tree exceeds the {limit} entry limit")]
    EntryLimit { limit: usize },
    #[error("Skill tree exceeds the {limit} byte limit")]
    ByteLimit { limit: u64 },
    #[error("Skill filesystem I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Skill recovery failed: {message}")]
    Recovery { message: String },
}

impl SkillConfigError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug)]
enum DeploymentExpectation {
    Missing,
    Linked { target: PathBuf },
    Copied { digest: String },
}

#[derive(Debug)]
enum DeploymentChange {
    Observed {
        destination: PathBuf,
        expectation: DeploymentExpectation,
    },
    Created {
        destination: PathBuf,
        expectation: DeploymentExpectation,
    },
    Removed {
        destination: PathBuf,
        temporary_root: PathBuf,
        backup: PathBuf,
        expectation: DeploymentExpectation,
    },
}

/// A reversible native Skill change.
#[derive(Debug)]
#[must_use = "a Skill deployment receipt must be committed or rolled back"]
pub struct SkillDeploymentReceipt {
    change: DeploymentChange,
}

impl SkillDeploymentReceipt {
    /// Verifies that the native destination still matches the applied change.
    pub fn verify(&self) -> Result<(), SkillConfigError> {
        match &self.change {
            DeploymentChange::Observed {
                destination,
                expectation,
            }
            | DeploymentChange::Created {
                destination,
                expectation,
            } => require_expectation(destination, expectation),
            DeploymentChange::Removed {
                destination,
                backup,
                expectation,
                ..
            } => {
                require_expectation(destination, &DeploymentExpectation::Missing)?;
                require_expectation(backup, expectation)
            }
        }
    }

    /// Finalizes an applied change after the host commits its catalog transaction.
    pub fn commit(self) -> Result<(), SkillConfigError> {
        self.verify()?;
        if let DeploymentChange::Removed { temporary_root, .. } = self.change {
            remove_directory(&temporary_root)?;
        }
        Ok(())
    }

    /// Restores the previous native state after the host rolls back its transaction.
    pub fn rollback(self) -> Result<(), SkillConfigError> {
        match self.change {
            DeploymentChange::Observed { .. } => Ok(()),
            DeploymentChange::Created {
                destination,
                expectation,
            } => {
                require_expectation(&destination, &expectation)?;
                remove_deployment(&destination)
            }
            DeploymentChange::Removed {
                destination,
                temporary_root,
                backup,
                expectation,
            } => {
                require_expectation(&destination, &DeploymentExpectation::Missing)?;
                require_expectation(&backup, &expectation)?;
                fs::rename(&backup, &destination)
                    .map_err(|source| SkillConfigError::io(&destination, source))?;
                fs::remove_dir(&temporary_root)
                    .map_err(|source| SkillConfigError::io(&temporary_root, source))
            }
        }
    }
}

/// Validates the single path component stored in the shared Skill catalog.
pub fn validate_skill_directory(directory: &str) -> Result<(), SkillConfigError> {
    if directory.is_empty()
        || directory.trim() != directory
        || directory.starts_with('.')
        || directory.contains('/')
        || directory.contains('\\')
    {
        return Err(SkillConfigError::InvalidDirectory {
            directory: directory.to_owned(),
        });
    }
    let mut components = Path::new(directory).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(SkillConfigError::InvalidDirectory {
            directory: directory.to_owned(),
        });
    }
    Ok(())
}

/// Computes a deterministic digest of every regular file and directory in a Skill.
pub fn skill_tree_digest(path: &Path) -> Result<String, SkillConfigError> {
    validate_skill_source(path)?;
    tree_digest(path)
}

/// Inspects a native destination without changing it.
pub fn inspect_skill_deployment(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
) -> Result<SkillDeploymentState, SkillConfigError> {
    let paths = deployment_paths(source_root, destination_root, directory)?;
    inspect_paths(&paths.source, &paths.destination).map(|(state, _)| state)
}

/// Applies an enable or disable operation and returns a reversible receipt.
pub fn apply_skill_deployment(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
    enabled: bool,
    sync_method: SkillSyncMethod,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let paths = deployment_paths(source_root, destination_root, directory)?;
    let (state, expectation) = inspect_paths(&paths.source, &paths.destination)?;
    if enabled {
        return match state {
            SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
                Ok(observed_receipt(paths.destination, expectation))
            }
            SkillDeploymentState::Missing => enable_deployment(paths, sync_method),
        };
    }

    match state {
        SkillDeploymentState::Missing => Ok(observed_receipt(paths.destination, expectation)),
        SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
            disable_deployment(paths.destination, expectation)
        }
    }
}

struct DeploymentPaths {
    source: PathBuf,
    destination: PathBuf,
}

fn deployment_paths(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
) -> Result<DeploymentPaths, SkillConfigError> {
    validate_skill_directory(directory)?;
    for root in [source_root, destination_root] {
        if !root.is_absolute() {
            return Err(SkillConfigError::RelativeRoot {
                path: root.to_owned(),
            });
        }
    }
    ensure_distinct_roots(source_root, destination_root)?;
    let source = source_root.join(directory);
    validate_skill_source(&source)?;
    Ok(DeploymentPaths {
        source,
        destination: destination_root.join(directory),
    })
}

/// Validates that an installed Skill is a real directory with a regular `SKILL.md`.
pub fn validate_skill_source(source: &Path) -> Result<(), SkillConfigError> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(SkillConfigError::UnsupportedEntry {
                path: source.to_owned(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillConfigError::MissingSource {
                path: source.to_owned(),
            })
        }
        Err(source_error) => return Err(SkillConfigError::io(source, source_error)),
    }
    let manifest = source.join("SKILL.md");
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SkillConfigError::MissingManifest { path: manifest }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(SkillConfigError::MissingManifest { path: manifest })
        }
        Err(source) => Err(SkillConfigError::io(&manifest, source)),
    }
}

fn ensure_distinct_roots(source: &Path, destination: &Path) -> Result<(), SkillConfigError> {
    let source = resolve_candidate(source)?;
    let destination = resolve_candidate(destination)?;
    if source == destination || source.starts_with(&destination) || destination.starts_with(&source)
    {
        return Err(SkillConfigError::OverlappingRoots {
            source_root: source,
            destination_root: destination,
        });
    }
    Ok(())
}

fn resolve_candidate(path: &Path) -> Result<PathBuf, SkillConfigError> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| SkillConfigError::Io {
                    path: path.to_owned(),
                    source: error,
                })?;
                suffix.push(name.to_os_string());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| SkillConfigError::RelativeRoot {
                        path: path.to_owned(),
                    })?;
            }
            Err(source) => return Err(SkillConfigError::io(ancestor, source)),
        }
    }
}

fn inspect_paths(
    source: &Path,
    destination: &Path,
) -> Result<(SkillDeploymentState, DeploymentExpectation), SkillConfigError> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                SkillDeploymentState::Missing,
                DeploymentExpectation::Missing,
            ))
        }
        Err(source_error) => return Err(SkillConfigError::io(destination, source_error)),
    };

    if metadata.file_type().is_symlink() {
        let expected = fs::canonicalize(source)
            .map_err(|source_error| SkillConfigError::io(source, source_error))?;
        let target = fs::read_link(destination)
            .map_err(|source_error| SkillConfigError::io(destination, source_error))?;
        let resolved = if target.is_absolute() {
            target.clone()
        } else {
            destination
                .parent()
                .expect("a deployment has a destination root")
                .join(&target)
        };
        if canonicalize_optional(&resolved)?.as_ref() == Some(&expected) {
            return Ok((
                SkillDeploymentState::Linked,
                DeploymentExpectation::Linked { target },
            ));
        }
        return Err(SkillConfigError::Conflict {
            path: destination.to_owned(),
        });
    }

    if metadata.file_type().is_dir() {
        let source_digest = tree_digest(source)?;
        let destination_digest = tree_digest(destination)?;
        if source_digest == destination_digest {
            return Ok((
                SkillDeploymentState::Copied,
                DeploymentExpectation::Copied {
                    digest: destination_digest,
                },
            ));
        }
    }

    Err(SkillConfigError::Conflict {
        path: destination.to_owned(),
    })
}

fn enable_deployment(
    paths: DeploymentPaths,
    sync_method: SkillSyncMethod,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    fs::create_dir_all(parent).map_err(|source| SkillConfigError::io(parent, source))?;

    if !matches!(sync_method, SkillSyncMethod::Copy) {
        match create_symlink(&paths.source, &paths.destination) {
            Ok(()) => {
                let receipt = SkillDeploymentReceipt {
                    change: DeploymentChange::Created {
                        destination: paths.destination,
                        expectation: DeploymentExpectation::Linked {
                            target: paths.source,
                        },
                    },
                };
                if let Err(error) = receipt.verify() {
                    return Err(recover_created(receipt, error));
                }
                return Ok(receipt);
            }
            Err(error) if matches!(sync_method, SkillSyncMethod::Symlink) => return Err(error),
            Err(_) => match inspect_paths(&paths.source, &paths.destination)? {
                (SkillDeploymentState::Missing, _) => {}
                (_, expectation) => {
                    return Ok(observed_receipt(paths.destination, expectation));
                }
            },
        }
    }

    create_copy(paths)
}

fn create_copy(paths: DeploymentPaths) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root = create_temporary_directory(parent)?;
    let staged = temporary_root.join("deployment");
    let staged_digest = (|| {
        let mut budget = TreeBudget::default();
        copy_tree(&paths.source, &staged, &mut budget)?;
        Ok((tree_digest(&paths.source)?, tree_digest(&staged)?))
    })();
    let (source_digest, staged_digest) = match staged_digest {
        Ok(digests) => digests,
        Err(error) => return Err(cleanup_temporary(&temporary_root, error)),
    };
    if staged_digest != source_digest {
        return Err(cleanup_temporary(
            &temporary_root,
            SkillConfigError::Conflict { path: paths.source },
        ));
    }
    if let Err(source) = fs::rename(&staged, &paths.destination) {
        return Err(cleanup_temporary(
            &temporary_root,
            SkillConfigError::io(&paths.destination, source),
        ));
    }
    let receipt = SkillDeploymentReceipt {
        change: DeploymentChange::Created {
            destination: paths.destination,
            expectation: DeploymentExpectation::Copied {
                digest: source_digest,
            },
        },
    };
    if let Err(source) = fs::remove_dir(&temporary_root) {
        let error = recover_created(receipt, SkillConfigError::io(&temporary_root, source));
        return Err(cleanup_temporary(&temporary_root, error));
    }
    if let Err(error) = receipt.verify() {
        return Err(recover_created(receipt, error));
    }
    Ok(receipt)
}

fn recover_created(receipt: SkillDeploymentReceipt, error: SkillConfigError) -> SkillConfigError {
    match receipt.rollback() {
        Ok(()) => error,
        Err(rollback) => SkillConfigError::Recovery {
            message: format!("{error}; rollback: {rollback}"),
        },
    }
}

fn disable_deployment(
    destination: PathBuf,
    expectation: DeploymentExpectation,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root = create_temporary_directory(parent)?;
    let backup = temporary_root.join("deployment");
    if let Err(source) = fs::rename(&destination, &backup) {
        return Err(cleanup_temporary(
            &temporary_root,
            SkillConfigError::io(&destination, source),
        ));
    }
    let receipt = SkillDeploymentReceipt {
        change: DeploymentChange::Removed {
            destination,
            temporary_root,
            backup,
            expectation,
        },
    };
    if let Err(error) = receipt.verify() {
        return Err(recover_removed(receipt, error));
    }
    Ok(receipt)
}

fn recover_removed(receipt: SkillDeploymentReceipt, error: SkillConfigError) -> SkillConfigError {
    match receipt.rollback() {
        Ok(()) => error,
        Err(rollback) => SkillConfigError::Recovery {
            message: format!("{error}; rollback: {rollback}"),
        },
    }
}

fn observed_receipt(
    destination: PathBuf,
    expectation: DeploymentExpectation,
) -> SkillDeploymentReceipt {
    SkillDeploymentReceipt {
        change: DeploymentChange::Observed {
            destination,
            expectation,
        },
    }
}

fn cleanup_temporary(path: &Path, error: SkillConfigError) -> SkillConfigError {
    match remove_directory(path) {
        Ok(()) => error,
        Err(cleanup) => SkillConfigError::Recovery {
            message: format!("{error}; temporary cleanup: {cleanup}"),
        },
    }
}

fn require_expectation(
    path: &Path,
    expectation: &DeploymentExpectation,
) -> Result<(), SkillConfigError> {
    let matches = match expectation {
        DeploymentExpectation::Missing => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Ok(_) => false,
            Err(source) => return Err(SkillConfigError::io(path, source)),
        },
        DeploymentExpectation::Linked { target } => {
            let metadata = fs::symlink_metadata(path)
                .map_err(|source_error| SkillConfigError::io(path, source_error))?;
            if !metadata.file_type().is_symlink() {
                false
            } else {
                fs::read_link(path)
                    .map_err(|source_error| SkillConfigError::io(path, source_error))?
                    == *target
            }
        }
        DeploymentExpectation::Copied { digest } => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => tree_digest(path)? == *digest,
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => return Err(SkillConfigError::io(path, source)),
        },
    };
    if matches {
        Ok(())
    } else {
        Err(SkillConfigError::Conflict {
            path: path.to_owned(),
        })
    }
}

fn canonicalize_optional(path: &Path) -> Result<Option<PathBuf>, SkillConfigError> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SkillConfigError::io(path, source)),
    }
}

fn tree_digest(root: &Path) -> Result<String, SkillConfigError> {
    let metadata =
        fs::symlink_metadata(root).map_err(|source| SkillConfigError::io(root, source))?;
    if !metadata.file_type().is_dir() {
        return Err(SkillConfigError::UnsupportedEntry {
            path: root.to_owned(),
        });
    }
    let mut budget = TreeBudget::default();
    let mut hasher = Sha256::new();
    hash_directory(root, root, &mut budget, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Default)]
struct TreeBudget {
    entries: usize,
    bytes: u64,
}

impl TreeBudget {
    fn add_entry(&mut self) -> Result<(), SkillConfigError> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_SKILL_TREE_ENTRIES {
            return Err(SkillConfigError::EntryLimit {
                limit: MAX_SKILL_TREE_ENTRIES,
            });
        }
        Ok(())
    }

    fn add_bytes(&mut self, bytes: u64) -> Result<(), SkillConfigError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_SKILL_TREE_BYTES {
            return Err(SkillConfigError::ByteLimit {
                limit: MAX_SKILL_TREE_BYTES,
            });
        }
        Ok(())
    }
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    budget: &mut TreeBudget,
    hasher: &mut Sha256,
) -> Result<(), SkillConfigError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| SkillConfigError::io(directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SkillConfigError::io(directory, source))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        budget.add_entry()?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("a traversed entry is under its root");
        let relative = relative
            .to_str()
            .ok_or_else(|| SkillConfigError::UnsupportedEntry { path: path.clone() })?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| SkillConfigError::io(&path, source))?;
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
            return Err(SkillConfigError::UnsupportedEntry { path });
        }
    }
    Ok(())
}

fn hash_file(
    path: &Path,
    before: &fs::Metadata,
    budget: &mut TreeBudget,
    hasher: &mut Sha256,
) -> Result<(), SkillConfigError> {
    let mut file = File::open(path).map_err(|source| SkillConfigError::io(path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hasher.update((before.permissions().mode() & 0o777).to_le_bytes());
    }
    #[cfg(not(unix))]
    hasher.update([u8::from(before.permissions().readonly())]);

    let mut read_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| SkillConfigError::io(path, source))?;
        if count == 0 {
            break;
        }
        let count = u64::try_from(count).expect("buffer length fits u64");
        budget.add_bytes(count)?;
        read_bytes = read_bytes.saturating_add(count);
        hasher.update(&buffer[..usize::try_from(count).expect("read count fits usize")]);
    }
    let after = fs::symlink_metadata(path).map_err(|source| SkillConfigError::io(path, source))?;
    if !after.file_type().is_file() || before.len() != read_bytes || after.len() != read_bytes {
        return Err(SkillConfigError::Conflict {
            path: path.to_owned(),
        });
    }
    hasher.update(b"\0");
    Ok(())
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    budget: &mut TreeBudget,
) -> Result<(), SkillConfigError> {
    fs::create_dir(destination)
        .map_err(|source_error| SkillConfigError::io(destination, source_error))?;
    let mut entries = fs::read_dir(source)
        .map_err(|source_error| SkillConfigError::io(source, source_error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source_error| SkillConfigError::io(source, source_error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        budget.add_entry()?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|source_error| SkillConfigError::io(&source_path, source_error))?;
        if metadata.file_type().is_dir() {
            copy_tree(&source_path, &destination_path, budget)?;
        } else if metadata.file_type().is_file() {
            budget.add_bytes(metadata.len())?;
            let copied = fs::copy(&source_path, &destination_path)
                .map_err(|source_error| SkillConfigError::io(&destination_path, source_error))?;
            if copied != metadata.len() {
                return Err(SkillConfigError::Conflict { path: source_path });
            }
        } else {
            return Err(SkillConfigError::UnsupportedEntry { path: source_path });
        }
    }
    Ok(())
}

fn create_temporary_directory(parent: &Path) -> Result<PathBuf, SkillConfigError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut last_error = None;
    for _ in 0..16 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".cc-switch-skill.{}.{timestamp}.{counter}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some((path, source));
            }
            Err(source) => return Err(SkillConfigError::io(path, source)),
        }
    }
    let (path, source) = last_error.expect("temporary directory loop must run");
    Err(SkillConfigError::io(path, source))
}

#[cfg(unix)]
fn create_symlink(source: &Path, destination: &Path) -> Result<(), SkillConfigError> {
    std::os::unix::fs::symlink(source, destination)
        .map_err(|error| SkillConfigError::io(destination, error))
}

#[cfg(windows)]
fn create_symlink(source: &Path, destination: &Path) -> Result<(), SkillConfigError> {
    std::os::windows::fs::symlink_dir(source, destination)
        .map_err(|error| SkillConfigError::io(destination, error))
}

fn remove_deployment(path: &Path) -> Result<(), SkillConfigError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        fs::remove_file(path).map_err(|source| SkillConfigError::io(path, source))
    } else if metadata.file_type().is_dir() {
        remove_directory(path)
    } else {
        Err(SkillConfigError::UnsupportedEntry {
            path: path.to_owned(),
        })
    }
}

fn remove_directory(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_dir_all(path).map_err(|source| SkillConfigError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(source.join("docs")).unwrap();
        fs::write(source.join("docs/SKILL.md"), "# Docs\n").unwrap();
        (temporary, source, destination)
    }

    #[test]
    fn directory_names_are_single_normalized_components() {
        validate_skill_directory("docs").unwrap();
        for invalid in ["", " docs", "docs ", ".docs", "../docs", "a/b", "a\\b"] {
            assert!(validate_skill_directory(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn copied_deployment_commits_and_disables_without_touching_source() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );

        apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert!(source.join("docs/SKILL.md").exists());
        assert!(!destination.join("docs").exists());
    }

    #[test]
    fn created_copy_can_be_rolled_back() {
        let (_temporary, source, destination) = roots();
        let receipt =
            apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
                .unwrap();
        receipt.rollback().unwrap();
        assert!(!destination.join("docs").exists());
    }

    #[test]
    fn removed_copy_can_be_rolled_back() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let receipt =
            apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
                .unwrap();
        assert!(!destination.join("docs").exists());
        receipt.rollback().unwrap();
        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );
    }

    #[test]
    fn conflicting_destination_is_preserved() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(destination.join("docs")).unwrap();
        fs::write(destination.join("docs/SKILL.md"), "external\n").unwrap();
        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy),
            Err(SkillConfigError::Conflict { .. })
        ));
        assert_eq!(
            fs::read_to_string(destination.join("docs/SKILL.md")).unwrap(),
            "external\n"
        );
    }

    #[test]
    fn overlapping_roots_are_rejected() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("skills");
        fs::create_dir_all(source.join("docs")).unwrap();
        fs::write(source.join("docs/SKILL.md"), "docs").unwrap();
        assert!(matches!(
            inspect_skill_deployment(&source, &source.join("nested"), "docs"),
            Err(SkillConfigError::OverlappingRoots { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn linked_deployment_is_verified_and_reversible() {
        let (_temporary, source, destination) = roots();
        let receipt = apply_skill_deployment(
            &source,
            &destination,
            "docs",
            true,
            SkillSyncMethod::Symlink,
        )
        .unwrap();
        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Linked
        );
        receipt.rollback().unwrap();
        assert!(!destination.join("docs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn relative_link_can_be_disabled_and_rolled_back() {
        use std::os::unix::fs::symlink;

        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        symlink("../source/docs", destination.join("docs")).unwrap();
        let receipt = apply_skill_deployment(
            &source,
            &destination,
            "docs",
            false,
            SkillSyncMethod::Symlink,
        )
        .unwrap();
        assert!(!destination.join("docs").exists());
        receipt.rollback().unwrap();
        assert_eq!(
            fs::read_link(destination.join("docs")).unwrap(),
            PathBuf::from("../source/docs")
        );
    }
}
