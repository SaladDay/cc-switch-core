use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{fs::sync_directory, AppType};

use super::read::{resolve_root, roots_overlap};

const OWNER_VERSION: u8 = 2;
const MAX_OWNER_BYTES: u64 = 4096;
static OWNER_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One Core-owned native Skill reference intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReferencePlan {
    skill_id: String,
    app: AppType,
    source_root: PathBuf,
    destination_root: PathBuf,
    state_root: PathBuf,
    directory: String,
    enabled: bool,
}

impl SkillReferencePlan {
    pub(super) fn new(
        skill_id: impl Into<String>,
        app: AppType,
        source_root: impl Into<PathBuf>,
        destination_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        directory: impl Into<String>,
        enabled: bool,
    ) -> Self {
        Self {
            skill_id: skill_id.into(),
            app,
            source_root: source_root.into(),
            destination_root: destination_root.into(),
            state_root: state_root.into(),
            directory: directory.into(),
            enabled,
        }
    }

    pub fn skill_id(&self) -> &str {
        &self.skill_id
    }

    pub fn app(&self) -> &AppType {
        &self.app
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub fn destination_root(&self) -> &Path {
        &self.destination_root
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Debug)]
struct ReferencePaths {
    source: PathBuf,
    destination: PathBuf,
    destination_fingerprint: String,
    owner: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceOwner {
    version: u8,
    skill_id: String,
    app: String,
    directory: String,
    destination: String,
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_target: Option<String>,
}

impl ReferenceOwner {
    fn stable(plan: &SkillReferencePlan, destination: String, target: String) -> Self {
        Self {
            version: OWNER_VERSION,
            skill_id: plan.skill_id.clone(),
            app: plan.app.as_str().to_owned(),
            directory: plan.directory.clone(),
            destination,
            target,
            pending_target: None,
        }
    }

    fn accepts(&self, target: &str) -> bool {
        self.target == target || self.pending_target.as_deref() == Some(target)
    }
}

#[derive(Debug)]
enum DestinationState {
    Missing,
    Reference {
        target: PathBuf,
        fingerprint: String,
        reachable: bool,
    },
    Other,
}

#[derive(Debug)]
enum ManagedState {
    Missing {
        owner: Option<ReferenceOwner>,
    },
    Reference {
        owner: ReferenceOwner,
        target: PathBuf,
        fingerprint: String,
    },
}

/// Read-only ownership classification used by the snapshot layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillReferenceObservation {
    Missing,
    ManagedPresent,
    ManagedMissing,
    Unmanaged,
    Conflict,
    Unreadable,
}

pub(super) fn inspect_skill_reference(
    state_root: &Path,
    app: &AppType,
    skill_id: &str,
    directory: &str,
    source: &Path,
    destination_root: &Path,
) -> SkillReferenceObservation {
    let plan = SkillReferencePlan::new(
        skill_id,
        app.clone(),
        source.parent().unwrap_or(source),
        destination_root,
        state_root,
        directory,
        true,
    );
    let destination_fingerprint = match destination_fingerprint(destination_root, directory) {
        Ok(fingerprint) => fingerprint,
        Err(_) => return SkillReferenceObservation::Unreadable,
    };
    let paths = ReferencePaths {
        source: source.to_owned(),
        destination: destination_root.join(directory),
        owner: owner_path(
            state_root,
            app,
            skill_id,
            directory,
            &destination_fingerprint,
        ),
        destination_fingerprint,
    };
    let owner = match read_owner(&paths.owner, &plan, &paths.destination_fingerprint) {
        Ok(owner) => owner,
        Err(SkillReferenceError::Io { .. }) => return SkillReferenceObservation::Unreadable,
        Err(_) => return SkillReferenceObservation::Conflict,
    };
    let destination = match inspect_destination(&paths.destination) {
        Ok(destination) => destination,
        Err(_) => return SkillReferenceObservation::Unreadable,
    };
    match (owner, destination) {
        (None, DestinationState::Missing) => SkillReferenceObservation::Missing,
        (None, _) => SkillReferenceObservation::Unmanaged,
        (Some(_), DestinationState::Missing) => SkillReferenceObservation::ManagedMissing,
        (Some(_), DestinationState::Other) => match is_incomplete_reference(&paths.destination) {
            Ok(true) => SkillReferenceObservation::ManagedMissing,
            Ok(false) => SkillReferenceObservation::Conflict,
            Err(_) => SkillReferenceObservation::Unreadable,
        },
        (
            Some(owner),
            DestinationState::Reference {
                fingerprint,
                reachable,
                ..
            },
        ) if owner.accepts(&fingerprint) => {
            if reachable {
                SkillReferenceObservation::ManagedPresent
            } else {
                SkillReferenceObservation::ManagedMissing
            }
        }
        (Some(_), _) => SkillReferenceObservation::Conflict,
    }
}

/// A native reference change awaiting the host's catalog decision.
#[derive(Debug)]
#[must_use = "a Skill reference receipt must be committed or rolled back"]
pub struct SkillReferenceReceipt {
    paths: ReferencePaths,
    plan: SkillReferencePlan,
    previous: ManagedState,
    target_fingerprint: String,
}

impl SkillReferenceReceipt {
    pub fn verify(&self) -> Result<(), SkillReferenceError> {
        if self.plan.enabled {
            let owner = read_owner(
                &self.paths.owner,
                &self.plan,
                &self.paths.destination_fingerprint,
            )?
            .ok_or_else(|| SkillReferenceError::Conflict {
                path: self.paths.owner.clone(),
            })?;
            if !owner.accepts(&self.target_fingerprint) {
                return Err(SkillReferenceError::Conflict {
                    path: self.paths.owner.clone(),
                });
            }
            require_reference(&self.paths.destination, &self.target_fingerprint)
        } else {
            require_missing(&self.paths.destination)
        }
    }

    pub fn commit(self) -> Result<(), SkillReferenceError> {
        self.verify()?;
        if self.plan.enabled {
            write_owner(
                &self.paths.owner,
                &ReferenceOwner::stable(
                    &self.plan,
                    self.paths.destination_fingerprint,
                    self.target_fingerprint,
                ),
            )
        } else {
            remove_owner_if_present(&self.paths.owner)
        }
    }

    pub fn rollback(self) -> Result<(), SkillReferenceError> {
        restore_previous(
            &self.paths,
            &self.plan,
            &self.previous,
            &self.target_fingerprint,
        )
    }
}

/// Applies one idempotent, owner-checked native reference intent.
///
/// Core never adopts or removes an unowned path. On Unix it creates a
/// symbolic link; on Windows it creates an NTFS junction, avoiding recursive
/// directory copies and deletes entirely. The host must first create
/// [`SkillReferencePlan::destination_root`] and
/// [`SkillReferencePlan::state_root`] as real directories while holding the
/// shared live-config lock; Core deliberately does not create host-owned roots.
pub fn apply_skill_reference(
    plan: &SkillReferencePlan,
) -> Result<SkillReferenceReceipt, SkillReferenceError> {
    let paths = prepare_paths(plan)?;
    let target = fs::canonicalize(&paths.source)
        .map_err(|source| SkillReferenceError::io(&paths.source, source))?;
    let target_fingerprint = path_fingerprint(&target);
    let previous = manageable_state(plan, &paths)?;

    let applied = if plan.enabled {
        apply_enabled(plan, &paths, &previous, &target, &target_fingerprint)
    } else {
        apply_disabled(&paths, &previous)
    };
    if let Err(error) = applied {
        return match restore_previous(&paths, plan, &previous, &target_fingerprint) {
            Ok(()) => Err(error),
            Err(rollback) => Err(SkillReferenceError::Recovery {
                message: format!("{error}; rollback failed: {rollback}"),
            }),
        };
    }

    Ok(SkillReferenceReceipt {
        paths,
        plan: plan.clone(),
        previous,
        target_fingerprint,
    })
}

fn prepare_paths(plan: &SkillReferencePlan) -> Result<ReferencePaths, SkillReferenceError> {
    for root in [&plan.source_root, &plan.destination_root, &plan.state_root] {
        if !root.is_absolute() {
            return Err(SkillReferenceError::RelativeRoot { path: root.clone() });
        }
    }
    let source = plan.source_root.join(&plan.directory);
    validate_source(&source)?;
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    require_real_directory(&plan.destination_root)?;
    require_real_directory(&plan.state_root)?;
    let destination_fingerprint = destination_fingerprint(&plan.destination_root, &plan.directory)?;
    let owner = owner_path(
        &plan.state_root,
        &plan.app,
        &plan.skill_id,
        &plan.directory,
        &destination_fingerprint,
    );
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    Ok(ReferencePaths {
        source,
        destination: plan.destination_root.join(&plan.directory),
        destination_fingerprint,
        owner,
    })
}

fn require_real_directory(path: &Path) -> Result<(), SkillReferenceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(SkillReferenceError::InvalidRoot {
            path: path.to_owned(),
        }),
        Err(source) => Err(SkillReferenceError::io(path, source)),
    }
}

fn validate_source(path: &Path) -> Result<(), SkillReferenceError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| SkillReferenceError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SkillReferenceError::InvalidSource {
            path: path.to_owned(),
        });
    }
    let manifest = path.join("SKILL.md");
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SkillReferenceError::InvalidSource { path: manifest }),
        Err(source) => Err(SkillReferenceError::io(manifest, source)),
    }
}

fn validate_root_pair(left: &Path, right: &Path) -> Result<(), SkillReferenceError> {
    let left_resolved = resolve_root(left).map_err(|error| SkillReferenceError::Root {
        message: error.to_string(),
    })?;
    let right_resolved = resolve_root(right).map_err(|error| SkillReferenceError::Root {
        message: error.to_string(),
    })?;
    if roots_overlap(&left_resolved, &right_resolved) {
        Err(SkillReferenceError::OverlappingRoots {
            left: left.to_owned(),
            right: right.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn manageable_state(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<ManagedState, SkillReferenceError> {
    let owner = read_owner(&paths.owner, plan, &paths.destination_fingerprint)?;
    match (owner, inspect_destination(&paths.destination)?) {
        (None, DestinationState::Missing) => Ok(ManagedState::Missing { owner: None }),
        (None, _) => Err(SkillReferenceError::Unowned {
            path: paths.destination.clone(),
        }),
        (Some(owner), DestinationState::Missing) => {
            Ok(ManagedState::Missing { owner: Some(owner) })
        }
        (Some(owner), DestinationState::Other) if is_incomplete_reference(&paths.destination)? => {
            remove_incomplete_reference(&paths.destination)?;
            Ok(ManagedState::Missing { owner: Some(owner) })
        }
        (
            Some(owner),
            DestinationState::Reference {
                target,
                fingerprint,
                ..
            },
        ) if owner.accepts(&fingerprint) => Ok(ManagedState::Reference {
            owner,
            target,
            fingerprint,
        }),
        (Some(_), _) => Err(SkillReferenceError::Conflict {
            path: paths.destination.clone(),
        }),
    }
}

fn apply_enabled(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
    previous: &ManagedState,
    target: &Path,
    target_fingerprint: &str,
) -> Result<(), SkillReferenceError> {
    match previous {
        ManagedState::Reference { fingerprint, .. } if fingerprint == target_fingerprint => Ok(()),
        ManagedState::Missing { owner: None } => {
            publish_owner(
                &paths.owner,
                &ReferenceOwner::stable(
                    plan,
                    paths.destination_fingerprint.clone(),
                    target_fingerprint.to_owned(),
                ),
            )?;
            create_reference(target, &paths.destination)
        }
        ManagedState::Missing { owner: Some(owner) } => {
            let working = owner_for_transition(owner, target_fingerprint);
            write_owner(&paths.owner, &working)?;
            create_reference(target, &paths.destination)
        }
        ManagedState::Reference {
            owner, fingerprint, ..
        } => {
            let normalized = ReferenceOwner {
                target: fingerprint.clone(),
                pending_target: Some(target_fingerprint.to_owned()),
                ..owner.clone()
            };
            write_owner(&paths.owner, &normalized)?;
            remove_reference(&paths.destination, fingerprint)?;
            create_reference(target, &paths.destination)
        }
    }
}

fn owner_for_transition(owner: &ReferenceOwner, target: &str) -> ReferenceOwner {
    if owner.target == target {
        ReferenceOwner {
            pending_target: None,
            ..owner.clone()
        }
    } else {
        ReferenceOwner {
            pending_target: Some(target.to_owned()),
            ..owner.clone()
        }
    }
}

fn apply_disabled(
    paths: &ReferencePaths,
    previous: &ManagedState,
) -> Result<(), SkillReferenceError> {
    if let ManagedState::Reference { fingerprint, .. } = previous {
        remove_reference(&paths.destination, fingerprint)?;
    }
    Ok(())
}

fn restore_previous(
    paths: &ReferencePaths,
    plan: &SkillReferencePlan,
    previous: &ManagedState,
    applied_target: &str,
) -> Result<(), SkillReferenceError> {
    match previous {
        ManagedState::Missing { owner } => {
            match inspect_destination(&paths.destination)? {
                DestinationState::Missing => {}
                DestinationState::Reference { fingerprint, .. }
                    if fingerprint == applied_target =>
                {
                    remove_reference(&paths.destination, applied_target)?;
                }
                DestinationState::Other if is_incomplete_reference(&paths.destination)? => {
                    remove_incomplete_reference(&paths.destination)?;
                }
                _ => {
                    return Err(SkillReferenceError::Conflict {
                        path: paths.destination.clone(),
                    })
                }
            }
            restore_owner(
                &paths.owner,
                plan,
                &paths.destination_fingerprint,
                owner.as_ref(),
            )
        }
        ManagedState::Reference {
            owner,
            target,
            fingerprint,
        } => {
            match inspect_destination(&paths.destination)? {
                DestinationState::Missing => create_reference(target, &paths.destination)?,
                DestinationState::Reference {
                    fingerprint: actual,
                    ..
                } if actual == *fingerprint => {}
                DestinationState::Reference {
                    fingerprint: actual,
                    ..
                } if actual == applied_target => {
                    remove_reference(&paths.destination, applied_target)?;
                    create_reference(target, &paths.destination)?;
                }
                _ => {
                    return Err(SkillReferenceError::Conflict {
                        path: paths.destination.clone(),
                    })
                }
            }
            restore_owner(
                &paths.owner,
                plan,
                &paths.destination_fingerprint,
                Some(owner),
            )
        }
    }
}

fn restore_owner(
    path: &Path,
    plan: &SkillReferencePlan,
    destination_fingerprint: &str,
    owner: Option<&ReferenceOwner>,
) -> Result<(), SkillReferenceError> {
    match owner {
        Some(owner) => write_owner(path, owner),
        None => match read_owner(path, plan, destination_fingerprint)? {
            Some(_) => remove_owner_if_present(path),
            None => Ok(()),
        },
    }
}

fn owner_path(
    state_root: &Path,
    app: &AppType,
    skill_id: &str,
    directory: &str,
    destination_fingerprint: &str,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-skill-reference-v2\0");
    hasher.update(app.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(skill_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(directory.as_bytes());
    hasher.update(b"\0");
    hasher.update(destination_fingerprint.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    state_root.join(format!("{}.json", encode_digest(digest)))
}

fn destination_fingerprint(
    destination_root: &Path,
    directory: &str,
) -> Result<String, SkillReferenceError> {
    let root = resolve_root(destination_root).map_err(|error| SkillReferenceError::Root {
        message: error.to_string(),
    })?;
    Ok(path_fingerprint(&root.join(directory)))
}

fn read_owner(
    path: &Path,
    plan: &SkillReferencePlan,
    destination_fingerprint: &str,
) -> Result<Option<ReferenceOwner>, SkillReferenceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => {
            return Err(SkillReferenceError::InvalidOwner {
                path: path.to_owned(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    if metadata.len() > MAX_OWNER_BYTES {
        return Err(SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| file.take(MAX_OWNER_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|source| SkillReferenceError::io(path, source))?;
    let owner: ReferenceOwner =
        serde_json::from_slice(&bytes).map_err(|_| SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        })?;
    if bytes.len() as u64 > MAX_OWNER_BYTES
        || owner.version != OWNER_VERSION
        || owner.skill_id != plan.skill_id
        || owner.app != plan.app.as_str()
        || owner.directory != plan.directory
        || owner.destination != destination_fingerprint
        || !valid_digest(&owner.destination)
        || !valid_digest(&owner.target)
        || owner
            .pending_target
            .as_deref()
            .is_some_and(|target| !valid_digest(target) || target == owner.target)
    {
        return Err(SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        });
    }
    Ok(Some(owner))
}

fn owner_bytes(owner: &ReferenceOwner, path: &Path) -> Result<Vec<u8>, SkillReferenceError> {
    let bytes = serde_json::to_vec(owner).map_err(|_| SkillReferenceError::InvalidOwner {
        path: path.to_owned(),
    })?;
    if bytes.len() as u64 > MAX_OWNER_BYTES {
        return Err(SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        });
    }
    Ok(bytes)
}

fn publish_owner(path: &Path, owner: &ReferenceOwner) -> Result<(), SkillReferenceError> {
    let bytes = owner_bytes(owner, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary = None;
    for _ in 0..16 {
        let counter = OWNER_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".cc-switch-owner.{}.{timestamp}.{counter}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(mut file) => {
                if let Err(source) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(SkillReferenceError::io(&candidate, source));
                }
                temporary = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(SkillReferenceError::io(candidate, source)),
        }
    }
    let temporary = temporary.ok_or_else(|| SkillReferenceError::Conflict {
        path: path.to_owned(),
    })?;
    let published = fs::hard_link(&temporary, path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            SkillReferenceError::Conflict {
                path: path.to_owned(),
            }
        } else {
            SkillReferenceError::io(path, source)
        }
    });
    let _ = fs::remove_file(&temporary);
    published?;
    sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
}

fn write_owner(path: &Path, owner: &ReferenceOwner) -> Result<(), SkillReferenceError> {
    let bytes = owner_bytes(owner, path)?;
    crate::fs::atomic_write_private(path, &bytes).map_err(|error| {
        SkillReferenceError::OwnerWrite {
            path: path.to_owned(),
            message: error.to_string(),
        }
    })?;
    let parent = path.parent().expect("owner path has a parent");
    sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
}

fn remove_owner_if_present(path: &Path) -> Result<(), SkillReferenceError> {
    match fs::remove_file(path) {
        Ok(()) => {
            let parent = path.parent().expect("owner path has a parent");
            sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SkillReferenceError::io(path, source)),
    }
}

fn inspect_destination(path: &Path) -> Result<DestinationState, SkillReferenceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DestinationState::Missing)
        }
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    if !is_reference(path, &metadata)? {
        return Ok(DestinationState::Other);
    }
    let declared_target = reference_target(path)?;
    let (target, reachable) = match fs::canonicalize(&declared_target) {
        Ok(target) => (target, true),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && declared_target.is_absolute() =>
        {
            let target =
                resolve_root(&declared_target).map_err(|error| SkillReferenceError::Root {
                    message: error.to_string(),
                })?;
            (target, false)
        }
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    Ok(DestinationState::Reference {
        fingerprint: path_fingerprint(&target),
        target,
        reachable,
    })
}

fn require_reference(path: &Path, fingerprint: &str) -> Result<(), SkillReferenceError> {
    match inspect_destination(path)? {
        DestinationState::Reference {
            fingerprint: actual,
            ..
        } if actual == fingerprint => Ok(()),
        _ => Err(SkillReferenceError::Conflict {
            path: path.to_owned(),
        }),
    }
}

fn require_missing(path: &Path) -> Result<(), SkillReferenceError> {
    match inspect_destination(path)? {
        DestinationState::Missing => Ok(()),
        _ => Err(SkillReferenceError::Conflict {
            path: path.to_owned(),
        }),
    }
}

#[cfg(unix)]
fn is_reference(_path: &Path, metadata: &fs::Metadata) -> Result<bool, SkillReferenceError> {
    Ok(metadata.file_type().is_symlink())
}

#[cfg(windows)]
fn is_reference(path: &Path, metadata: &fs::Metadata) -> Result<bool, SkillReferenceError> {
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    match junction::get_target(path) {
        Ok(_) => Ok(true),
        Err(error)
            if error.raw_os_error().is_none() && error.kind() == std::io::ErrorKind::Other =>
        {
            Ok(false)
        }
        Err(source) => Err(SkillReferenceError::io(path, source)),
    }
}

#[cfg(not(any(unix, windows)))]
fn is_reference(_path: &Path, _metadata: &fs::Metadata) -> Result<bool, SkillReferenceError> {
    Ok(false)
}

#[cfg(unix)]
fn reference_target(path: &Path) -> Result<PathBuf, SkillReferenceError> {
    let target = fs::read_link(path).map_err(|source| SkillReferenceError::io(path, source))?;
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(path.parent().unwrap_or_else(|| Path::new(".")).join(target))
    }
}

#[cfg(windows)]
fn reference_target(path: &Path) -> Result<PathBuf, SkillReferenceError> {
    junction::get_target(path).map_err(|source| SkillReferenceError::io(path, source))
}

#[cfg(not(any(unix, windows)))]
fn reference_target(path: &Path) -> Result<PathBuf, SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: path.to_owned(),
    })
}

#[cfg(unix)]
fn create_reference(target: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    std::os::unix::fs::symlink(target, destination)
        .map_err(|source| SkillReferenceError::io(destination, source))?;
    let parent = destination.parent().expect("destination has a parent");
    sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
}

#[cfg(windows)]
fn create_reference(target: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    junction::create(target, destination)
        .map_err(|source| SkillReferenceError::io(destination, source))
}

#[cfg(not(any(unix, windows)))]
fn create_reference(_target: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: destination.to_owned(),
    })
}

fn remove_reference(path: &Path, fingerprint: &str) -> Result<(), SkillReferenceError> {
    require_reference(path, fingerprint)?;
    remove_reference_unchecked(path)?;
    let parent = path.parent().expect("reference has a parent");
    sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
}

#[cfg(unix)]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    fs::remove_file(path).map_err(|source| SkillReferenceError::io(path, source))
}

#[cfg(windows)]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    // RemoveDirectoryW deletes the junction directory entry itself. The
    // junction crate's `delete` only clears the reparse point and leaves an
    // empty ordinary directory behind.
    fs::remove_dir(path).map_err(|source| SkillReferenceError::io(path, source))
}

#[cfg(not(any(unix, windows)))]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: path.to_owned(),
    })
}

#[cfg(windows)]
fn is_incomplete_reference(path: &Path) -> Result<bool, SkillReferenceError> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let metadata =
        fs::symlink_metadata(path).map_err(|source| SkillReferenceError::io(path, source))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Ok(false);
    }
    let mut entries = fs::read_dir(path).map_err(|source| SkillReferenceError::io(path, source))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(source)) => Err(SkillReferenceError::io(path, source)),
    }
}

#[cfg(not(windows))]
fn is_incomplete_reference(_path: &Path) -> Result<bool, SkillReferenceError> {
    Ok(false)
}

fn remove_incomplete_reference(path: &Path) -> Result<(), SkillReferenceError> {
    fs::remove_dir(path).map_err(|source| SkillReferenceError::io(path, source))?;
    let parent = path.parent().expect("reference has a parent");
    sync_directory(parent).map_err(|source| SkillReferenceError::io(parent, source))
}

fn path_fingerprint(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-path-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    encode_digest(hasher.finalize().into())
}

fn encode_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A native Skill reference could not be inspected or changed safely.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillReferenceError {
    #[error("Skill root must be absolute: {path:?}")]
    RelativeRoot { path: PathBuf },
    #[error("Skill source is invalid: {path:?}")]
    InvalidSource { path: PathBuf },
    #[error("Skill reference root is not a real directory: {path:?}")]
    InvalidRoot { path: PathBuf },
    #[error("Skill roots overlap: {left:?}, {right:?}")]
    OverlappingRoots { left: PathBuf, right: PathBuf },
    #[error("Skill root validation failed: {message}")]
    Root { message: String },
    #[error("native Skill path is not owned by Core: {path:?}")]
    Unowned { path: PathBuf },
    #[error("native Skill reference conflicts with managed state: {path:?}")]
    Conflict { path: PathBuf },
    #[error("invalid Skill reference owner record: {path:?}")]
    InvalidOwner { path: PathBuf },
    #[error("Skill reference owner write failed at {path:?}: {message}")]
    OwnerWrite { path: PathBuf, message: String },
    #[error("Skill references are unsupported on this platform: {path:?}")]
    UnsupportedPlatform { path: PathBuf },
    #[error("Skill reference I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Skill reference recovery is incomplete: {message}")]
    Recovery { message: String },
}

impl SkillReferenceError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        let state = temporary.path().join("state");
        fs::create_dir_all(source.join("demo")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(source.join("demo/SKILL.md"), "# Demo\n").unwrap();
        (temporary, source, destination, state)
    }

    fn plan(source: &Path, destination: &Path, state: &Path, enabled: bool) -> SkillReferencePlan {
        SkillReferencePlan::new(
            "owner/repo:demo",
            AppType::Claude,
            source,
            destination,
            state,
            "demo",
            enabled,
        )
    }

    #[test]
    fn managed_references_are_idempotent_and_reversible() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        let disabled = plan(&source, &destination, &state, false);
        let receipt = apply_skill_reference(&disabled).unwrap();
        assert!(!destination.join("demo").exists());
        receipt.rollback().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        assert!(!destination.join("demo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_unowned_same_source_link_is_never_adopted_or_removed() {
        let (_temporary, source, destination, state) = roots();
        fs::create_dir_all(&destination).unwrap();
        std::os::unix::fs::symlink(source.join("demo"), destination.join("demo")).unwrap();
        let disabled = plan(&source, &destination, &state, false);

        assert!(matches!(
            apply_skill_reference(&disabled),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
    }

    #[test]
    fn an_unowned_directory_is_never_removed() {
        let (_temporary, source, destination, state) = roots();
        fs::create_dir_all(destination.join("demo")).unwrap();
        fs::write(destination.join("demo/SKILL.md"), "# Demo\n").unwrap();
        let disabled = plan(&source, &destination, &state, false);

        assert!(matches!(
            apply_skill_reference(&disabled),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn ownership_does_not_transfer_between_native_roots() {
        let (temporary, source, destination_a, state) = roots();
        let enabled_a = plan(&source, &destination_a, &state, true);
        apply_skill_reference(&enabled_a).unwrap().commit().unwrap();

        let destination_b = temporary.path().join("destination-b");
        fs::create_dir(&destination_b).unwrap();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        create_reference(&target, &destination_b.join("demo")).unwrap();
        let disabled_b = plan(&source, &destination_b, &state, false);

        assert!(matches!(
            apply_skill_reference(&disabled_b),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination_b.join("demo/SKILL.md").is_file());
        assert_ne!(
            prepare_paths(&enabled_a).unwrap().owner,
            prepare_paths(&disabled_b).unwrap().owner
        );
    }

    #[test]
    fn an_owner_without_a_link_converges_from_committed_selection() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        publish_owner(
            &paths.owner,
            &ReferenceOwner::stable(
                &enabled,
                paths.destination_fingerprint.clone(),
                path_fingerprint(&target),
            ),
        )
        .unwrap();

        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        let disabled = plan(&source, &destination, &state, false);
        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        assert!(!paths.owner.exists());
    }

    #[cfg(windows)]
    #[test]
    fn an_owned_empty_junction_placeholder_is_recoverable() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        publish_owner(
            &paths.owner,
            &ReferenceOwner::stable(
                &enabled,
                paths.destination_fingerprint.clone(),
                path_fingerprint(&target),
            ),
        )
        .unwrap();
        fs::create_dir(&paths.destination).unwrap();

        assert_eq!(
            inspect_skill_reference(
                &state,
                &AppType::Claude,
                enabled.skill_id(),
                enabled.directory(),
                &paths.source,
                &destination,
            ),
            SkillReferenceObservation::ManagedMissing
        );
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        apply_skill_reference(&plan(&source, &destination, &state, false))
            .unwrap()
            .commit()
            .unwrap();
        assert!(matches!(
            fs::symlink_metadata(&paths.destination),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(unix)]
    #[test]
    fn source_relocation_is_reversible() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let old_target = fs::canonicalize(source.join("demo")).unwrap();

        let relocated = temporary.path().join("relocated");
        fs::create_dir_all(relocated.join("demo")).unwrap();
        fs::write(relocated.join("demo/SKILL.md"), "# Relocated\n").unwrap();
        let relocated_plan = plan(&relocated, &destination, &state, true);
        let receipt = apply_skill_reference(&relocated_plan).unwrap();
        assert_eq!(
            fs::canonicalize(destination.join("demo")).unwrap(),
            fs::canonicalize(relocated.join("demo")).unwrap()
        );
        receipt.rollback().unwrap();
        assert_eq!(
            fs::canonicalize(destination.join("demo")).unwrap(),
            old_target
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_broken_owned_link_converges_to_the_current_source() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        fs::remove_dir_all(&source).unwrap();

        let relocated = temporary.path().join("relocated");
        fs::create_dir_all(relocated.join("demo")).unwrap();
        fs::write(relocated.join("demo/SKILL.md"), "# Relocated\n").unwrap();
        let relocated_plan = plan(&relocated, &destination, &state, true);

        apply_skill_reference(&relocated_plan)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            fs::canonicalize(destination.join("demo")).unwrap(),
            fs::canonicalize(relocated.join("demo")).unwrap()
        );
    }
}
