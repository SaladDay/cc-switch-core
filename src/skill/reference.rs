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

const OWNER_VERSION: u8 = 3;
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

#[derive(Debug, Clone)]
struct ReferencePaths {
    source: PathBuf,
    destination: PathBuf,
    owner: PathBuf,
    anchor: PathBuf,
    stage: PathBuf,
    destination_fingerprint: String,
    anchor_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceOwner {
    version: u8,
    skill_id: String,
    app: String,
    directory: String,
    destination: String,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_source: Option<String>,
}

impl ReferenceOwner {
    fn stable(plan: &SkillReferencePlan, destination: String, source: String) -> Self {
        Self {
            version: OWNER_VERSION,
            skill_id: plan.skill_id.clone(),
            app: plan.app.as_str().to_owned(),
            directory: plan.directory.clone(),
            destination,
            source,
            pending_source: None,
        }
    }

    fn accepts_source(&self, source: &str) -> bool {
        self.source == source || self.pending_source.as_deref() == Some(source)
    }

    fn transitioning_to(&self, source: &str) -> Self {
        if self.source == source {
            Self {
                pending_source: None,
                ..self.clone()
            }
        } else {
            Self {
                pending_source: Some(source.to_owned()),
                ..self.clone()
            }
        }
    }
}

#[derive(Debug)]
enum ReferenceEntry {
    Missing,
    Reference {
        declared_target: PathBuf,
        declared_fingerprint: String,
        resolved_fingerprint: String,
        reachable: bool,
    },
    Incomplete,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedLocation {
    Missing,
    Destination,
    Stage,
    IncompleteStage,
}

#[derive(Debug, Clone)]
struct ManagedState {
    owner: Option<ReferenceOwner>,
    anchor_target: Option<PathBuf>,
    anchor_source: Option<String>,
    anchor_reachable: bool,
    location: ManagedLocation,
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
    let paths = match build_paths(&plan) {
        Ok(paths) => paths,
        Err(_) => return SkillReferenceObservation::Unreadable,
    };
    match managed_state(&plan, &paths) {
        Ok(ManagedState { owner: None, .. }) => SkillReferenceObservation::Missing,
        Ok(state) if state.location == ManagedLocation::Destination => {
            if state.anchor_source.is_some() && state.anchor_reachable {
                SkillReferenceObservation::ManagedPresent
            } else {
                SkillReferenceObservation::ManagedMissing
            }
        }
        Ok(_) => SkillReferenceObservation::ManagedMissing,
        Err(SkillReferenceError::Unowned { .. }) => SkillReferenceObservation::Unmanaged,
        Err(SkillReferenceError::Io { .. }) => SkillReferenceObservation::Unreadable,
        Err(_) => SkillReferenceObservation::Conflict,
    }
}

/// A native reference change awaiting the host's catalog decision.
#[derive(Debug)]
#[must_use = "a Skill reference receipt must be committed or rolled back"]
pub struct SkillReferenceReceipt {
    paths: ReferencePaths,
    plan: SkillReferencePlan,
    previous: ManagedState,
    target_source: String,
}

impl SkillReferenceReceipt {
    pub fn verify(&self) -> Result<(), SkillReferenceError> {
        let state = managed_state(&self.plan, &self.paths)?;
        if self.plan.enabled {
            let owner = state
                .owner
                .as_ref()
                .ok_or_else(|| SkillReferenceError::Conflict {
                    path: self.paths.owner.clone(),
                })?;
            if state.location != ManagedLocation::Destination
                || state.anchor_source.as_deref() != Some(&self.target_source)
                || !state.anchor_reachable
                || !owner.accepts_source(&self.target_source)
            {
                return Err(SkillReferenceError::Conflict {
                    path: self.paths.destination.clone(),
                });
            }
        } else if state.location != ManagedLocation::Missing {
            return Err(SkillReferenceError::Conflict {
                path: self.paths.destination.clone(),
            });
        }
        Ok(())
    }

    pub fn commit(self) -> Result<(), SkillReferenceError> {
        self.verify()
    }

    pub fn rollback(self) -> Result<(), SkillReferenceError> {
        restore_previous(&self.paths, &self.plan, &self.previous)
    }
}

/// Applies one idempotent, owner-checked native reference intent.
///
/// The host supplies a private state root on the same filesystem as the
/// native root and creates both roots while holding the shared live-config
/// lock. Core creates references only in that private root, then atomically
/// moves them into place without replacement. Removal first moves a reference
/// back to the private root and verifies its direct anchor before deleting it.
pub fn apply_skill_reference(
    plan: &SkillReferencePlan,
) -> Result<SkillReferenceReceipt, SkillReferenceError> {
    let paths = prepare_paths(plan)?;
    let source = fs::canonicalize(&paths.source)
        .map_err(|source| SkillReferenceError::io(&paths.source, source))?;
    let target_source = path_fingerprint(&source);
    let previous = managed_state(plan, &paths)?;

    let applied = if plan.enabled {
        converge_enabled(plan, &paths, &source, &target_source)
    } else {
        converge_disabled(plan, &paths)
    };
    if let Err(error) = applied {
        return match restore_previous(&paths, plan, &previous) {
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
        target_source,
    })
}

fn prepare_paths(plan: &SkillReferencePlan) -> Result<ReferencePaths, SkillReferenceError> {
    for root in [&plan.source_root, &plan.destination_root, &plan.state_root] {
        if !root.is_absolute() {
            return Err(SkillReferenceError::RelativeRoot { path: root.clone() });
        }
    }
    let paths = build_paths(plan)?;
    validate_source(&paths.source)?;
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    require_real_directory(&plan.destination_root)?;
    require_real_directory(&plan.state_root)?;
    require_same_filesystem(&plan.destination_root, &plan.state_root)?;
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    Ok(paths)
}

fn build_paths(plan: &SkillReferencePlan) -> Result<ReferencePaths, SkillReferenceError> {
    let destination_fingerprint = bound_path_fingerprint(&plan.destination_root, &plan.directory)?;
    let stem = owner_stem(
        &plan.app,
        &plan.skill_id,
        &plan.directory,
        &destination_fingerprint,
    );
    let anchor_name = format!("{stem}.anchor");
    let anchor_fingerprint = bound_path_fingerprint(&plan.state_root, &anchor_name)?;
    Ok(ReferencePaths {
        source: plan.source_root.join(&plan.directory),
        destination: plan.destination_root.join(&plan.directory),
        owner: plan.state_root.join(format!("{stem}.json")),
        anchor: plan.state_root.join(anchor_name),
        stage: plan.state_root.join(format!("{stem}.stage")),
        destination_fingerprint,
        anchor_fingerprint,
    })
}

fn managed_state(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<ManagedState, SkillReferenceError> {
    let owner = read_owner(&paths.owner, plan, &paths.destination_fingerprint)?;
    let destination = inspect_reference(&paths.destination)?;
    let stage = inspect_reference(&paths.stage)?;
    let anchor = inspect_reference(&paths.anchor)?;

    let Some(owner) = owner else {
        if matches!(destination, ReferenceEntry::Missing)
            && matches!(stage, ReferenceEntry::Missing)
            && matches!(anchor, ReferenceEntry::Missing)
        {
            return Ok(ManagedState {
                owner: None,
                anchor_target: None,
                anchor_source: None,
                anchor_reachable: false,
                location: ManagedLocation::Missing,
            });
        }
        return Err(SkillReferenceError::Unowned {
            path: first_present_path(paths, &destination, &stage, &anchor),
        });
    };

    let destination_present = require_direct_anchor(&destination, paths)?;
    let stage_present = match stage {
        ReferenceEntry::Incomplete => None,
        _ => require_direct_anchor(&stage, paths)?,
    };
    if destination_present.is_some() && stage_present.is_some() {
        return Err(SkillReferenceError::Conflict {
            path: paths.destination.clone(),
        });
    }

    let (anchor_target, anchor_source, anchor_reachable) = match anchor {
        ReferenceEntry::Missing | ReferenceEntry::Incomplete => (None, None, false),
        ReferenceEntry::Reference {
            declared_target,
            resolved_fingerprint,
            reachable,
            ..
        } if owner.accepts_source(&resolved_fingerprint) => {
            (Some(declared_target), Some(resolved_fingerprint), reachable)
        }
        _ => {
            return Err(SkillReferenceError::Conflict {
                path: paths.anchor.clone(),
            })
        }
    };

    let location = if destination_present.is_some() {
        ManagedLocation::Destination
    } else if stage_present.is_some() {
        ManagedLocation::Stage
    } else if matches!(stage, ReferenceEntry::Incomplete) {
        ManagedLocation::IncompleteStage
    } else {
        ManagedLocation::Missing
    };
    Ok(ManagedState {
        owner: Some(owner),
        anchor_target,
        anchor_source,
        anchor_reachable,
        location,
    })
}

fn first_present_path(
    paths: &ReferencePaths,
    destination: &ReferenceEntry,
    stage: &ReferenceEntry,
    anchor: &ReferenceEntry,
) -> PathBuf {
    if !matches!(destination, ReferenceEntry::Missing) {
        paths.destination.clone()
    } else if !matches!(stage, ReferenceEntry::Missing) {
        paths.stage.clone()
    } else {
        debug_assert!(!matches!(anchor, ReferenceEntry::Missing));
        paths.anchor.clone()
    }
}

fn require_direct_anchor<'a>(
    entry: &'a ReferenceEntry,
    paths: &ReferencePaths,
) -> Result<Option<&'a Path>, SkillReferenceError> {
    match entry {
        ReferenceEntry::Missing => Ok(None),
        ReferenceEntry::Reference {
            declared_target,
            declared_fingerprint,
            ..
        } if declared_fingerprint == &paths.anchor_fingerprint => {
            Ok(Some(declared_target.as_path()))
        }
        _ => Err(SkillReferenceError::Conflict {
            path: paths.destination.clone(),
        }),
    }
}

fn converge_enabled(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
    source: &Path,
    source_fingerprint: &str,
) -> Result<(), SkillReferenceError> {
    let mut state = managed_state(plan, paths)?;
    let owner = match state.owner.clone() {
        Some(owner) => {
            let working = owner.transitioning_to(source_fingerprint);
            if working != owner {
                write_owner(&paths.owner, &working)?;
            }
            working
        }
        None => {
            let owner = ReferenceOwner::stable(
                plan,
                paths.destination_fingerprint.clone(),
                source_fingerprint.to_owned(),
            );
            publish_owner(&paths.owner, &owner)?;
            owner
        }
    };

    if state.location == ManagedLocation::IncompleteStage {
        remove_incomplete_private_entry(&paths.stage)?;
        state.location = ManagedLocation::Missing;
    }

    if state.anchor_source.as_deref() != Some(source_fingerprint) || !state.anchor_reachable {
        if state.location == ManagedLocation::Destination {
            park_destination(paths)?;
            state.location = ManagedLocation::Stage;
        }
        remove_anchor_if_present(paths, &owner)?;
        create_private_reference(source, &paths.anchor)?;
    }

    match state.location {
        ManagedLocation::Destination => {}
        ManagedLocation::Stage => unpark_destination(paths)?,
        ManagedLocation::Missing => {
            create_private_reference(&paths.anchor, &paths.stage)?;
            unpark_destination(paths)?;
        }
        ManagedLocation::IncompleteStage => unreachable!("cleaned above"),
    }
    write_owner(
        &paths.owner,
        &ReferenceOwner::stable(
            plan,
            paths.destination_fingerprint.clone(),
            source_fingerprint.to_owned(),
        ),
    )
}

fn converge_disabled(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<(), SkillReferenceError> {
    let state = managed_state(plan, paths)?;
    match state.location {
        ManagedLocation::Destination => {
            park_destination(paths)?;
            remove_stage(paths)?;
        }
        ManagedLocation::Stage => remove_stage(paths)?,
        ManagedLocation::IncompleteStage => remove_incomplete_private_entry(&paths.stage)?,
        ManagedLocation::Missing => {}
    }
    Ok(())
}

fn restore_previous(
    paths: &ReferencePaths,
    plan: &SkillReferencePlan,
    previous: &ManagedState,
) -> Result<(), SkillReferenceError> {
    let current = managed_state(plan, paths)?;
    if current.location == ManagedLocation::Destination {
        park_destination(paths)?;
    }
    match inspect_reference(&paths.stage)? {
        ReferenceEntry::Reference { .. } => remove_stage(paths)?,
        ReferenceEntry::Incomplete => remove_incomplete_private_entry(&paths.stage)?,
        ReferenceEntry::Missing => {}
        ReferenceEntry::Other => {
            return Err(SkillReferenceError::Conflict {
                path: paths.stage.clone(),
            })
        }
    }

    if let Some(owner) = current.owner.as_ref() {
        remove_anchor_if_present(paths, owner)?;
    }
    if let Some(target) = previous.anchor_target.as_deref() {
        create_private_reference(target, &paths.anchor)?;
    }

    match previous.location {
        ManagedLocation::Destination => {
            create_private_reference(&paths.anchor, &paths.stage)?;
            unpark_destination(paths)?;
        }
        ManagedLocation::Stage => create_private_reference(&paths.anchor, &paths.stage)?,
        ManagedLocation::Missing | ManagedLocation::IncompleteStage => {}
    }

    match previous.owner.as_ref() {
        Some(owner) => write_owner(&paths.owner, owner),
        None => remove_owner_if_present(&paths.owner),
    }
}

fn park_destination(paths: &ReferencePaths) -> Result<(), SkillReferenceError> {
    require_missing(&paths.stage)?;
    move_noreplace(&paths.destination, &paths.stage)?;
    sync_move_parents(&paths.destination, &paths.stage)?;
    match inspect_reference(&paths.stage)? {
        ReferenceEntry::Reference {
            declared_fingerprint,
            ..
        } if declared_fingerprint == paths.anchor_fingerprint => Ok(()),
        _ => {
            let restore = move_noreplace(&paths.stage, &paths.destination)
                .and_then(|()| sync_move_parents(&paths.stage, &paths.destination));
            match restore {
                Ok(()) => Err(SkillReferenceError::Unowned {
                    path: paths.destination.clone(),
                }),
                Err(error) => Err(SkillReferenceError::Recovery {
                    message: format!(
                        "an unowned native entry was quarantined; restore failed: {error}"
                    ),
                }),
            }
        }
    }
}

fn unpark_destination(paths: &ReferencePaths) -> Result<(), SkillReferenceError> {
    require_direct_anchor(&inspect_reference(&paths.stage)?, paths)?.ok_or_else(|| {
        SkillReferenceError::Conflict {
            path: paths.stage.clone(),
        }
    })?;
    require_missing(&paths.destination)?;
    move_noreplace(&paths.stage, &paths.destination)?;
    sync_move_parents(&paths.stage, &paths.destination)
}

fn remove_stage(paths: &ReferencePaths) -> Result<(), SkillReferenceError> {
    require_direct_anchor(&inspect_reference(&paths.stage)?, paths)?.ok_or_else(|| {
        SkillReferenceError::Conflict {
            path: paths.stage.clone(),
        }
    })?;
    remove_reference_unchecked(&paths.stage)?;
    sync_parent(&paths.stage)
}

fn remove_anchor_if_present(
    paths: &ReferencePaths,
    owner: &ReferenceOwner,
) -> Result<(), SkillReferenceError> {
    match inspect_reference(&paths.anchor)? {
        ReferenceEntry::Missing => Ok(()),
        ReferenceEntry::Incomplete => remove_incomplete_private_entry(&paths.anchor),
        ReferenceEntry::Reference {
            resolved_fingerprint,
            ..
        } if owner.accepts_source(&resolved_fingerprint) => {
            remove_reference_unchecked(&paths.anchor)?;
            sync_parent(&paths.anchor)
        }
        _ => Err(SkillReferenceError::Conflict {
            path: paths.anchor.clone(),
        }),
    }
}

fn create_private_reference(target: &Path, path: &Path) -> Result<(), SkillReferenceError> {
    match create_reference(target, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            if matches!(inspect_reference(path), Ok(ReferenceEntry::Incomplete)) {
                let _ = remove_incomplete_private_entry(path);
            }
            Err(error)
        }
    }
}

fn require_missing(path: &Path) -> Result<(), SkillReferenceError> {
    if matches!(inspect_reference(path)?, ReferenceEntry::Missing) {
        Ok(())
    } else {
        Err(SkillReferenceError::Conflict {
            path: path.to_owned(),
        })
    }
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

#[cfg(unix)]
fn require_same_filesystem(left: &Path, right: &Path) -> Result<(), SkillReferenceError> {
    use std::os::unix::fs::MetadataExt;

    let left_metadata =
        fs::metadata(left).map_err(|source| SkillReferenceError::io(left, source))?;
    let right_metadata =
        fs::metadata(right).map_err(|source| SkillReferenceError::io(right, source))?;
    if left_metadata.dev() == right_metadata.dev() {
        Ok(())
    } else {
        Err(SkillReferenceError::DifferentFilesystems {
            native: left.to_owned(),
            state: right.to_owned(),
        })
    }
}

#[cfg(windows)]
fn require_same_filesystem(left: &Path, right: &Path) -> Result<(), SkillReferenceError> {
    let volume = |path: &Path| -> Result<String, SkillReferenceError> {
        let resolved =
            fs::canonicalize(path).map_err(|source| SkillReferenceError::io(path, source))?;
        resolved
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .ok_or_else(|| SkillReferenceError::InvalidRoot {
                path: path.to_owned(),
            })
    };
    if volume(left)? == volume(right)? {
        Ok(())
    } else {
        Err(SkillReferenceError::DifferentFilesystems {
            native: left.to_owned(),
            state: right.to_owned(),
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn require_same_filesystem(left: &Path, _right: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: left.to_owned(),
    })
}

fn inspect_reference(path: &Path) -> Result<ReferenceEntry, SkillReferenceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReferenceEntry::Missing)
        }
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    if is_incomplete_reference(path, &metadata)? {
        return Ok(ReferenceEntry::Incomplete);
    }
    if !is_reference(path, &metadata)? {
        return Ok(ReferenceEntry::Other);
    }
    let declared_target = normalized_declared_target(path)?;
    let declared_fingerprint = path_fingerprint(&declared_target);
    let (resolved_fingerprint, reachable) = match fs::canonicalize(&declared_target) {
        Ok(target) => (path_fingerprint(&target), true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let target =
                resolve_root(&declared_target).map_err(|error| SkillReferenceError::Root {
                    message: error.to_string(),
                })?;
            (path_fingerprint(&target), false)
        }
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    Ok(ReferenceEntry::Reference {
        declared_target,
        declared_fingerprint,
        resolved_fingerprint,
        reachable,
    })
}

fn normalized_declared_target(path: &Path) -> Result<PathBuf, SkillReferenceError> {
    let target = reference_target(path)?;
    let absolute = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
    let name = absolute
        .file_name()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: path.to_owned(),
        })?;
    let parent = absolute
        .parent()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: path.to_owned(),
        })?;
    let parent = resolve_root(parent).map_err(|error| SkillReferenceError::Root {
        message: error.to_string(),
    })?;
    Ok(parent.join(name))
}

fn bound_path_fingerprint(root: &Path, name: &str) -> Result<String, SkillReferenceError> {
    let root = resolve_root(root).map_err(|error| SkillReferenceError::Root {
        message: error.to_string(),
    })?;
    Ok(path_fingerprint(&root.join(name)))
}

fn owner_stem(
    app: &AppType,
    skill_id: &str,
    directory: &str,
    destination_fingerprint: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-skill-reference-v3\0");
    hasher.update(app.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(skill_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(directory.as_bytes());
    hasher.update(b"\0");
    hasher.update(destination_fingerprint.as_bytes());
    encode_digest(hasher.finalize().into())
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
        || !valid_digest(&owner.source)
        || owner
            .pending_source
            .as_deref()
            .is_some_and(|source| !valid_digest(source) || source == owner.source)
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
    sync_parent(path)
}

fn remove_owner_if_present(path: &Path) -> Result<(), SkillReferenceError> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SkillReferenceError::io(path, source)),
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
    fs::read_link(path).map_err(|source| SkillReferenceError::io(path, source))
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
    sync_parent(destination)
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

#[cfg(unix)]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    fs::remove_file(path).map_err(|source| SkillReferenceError::io(path, source))
}

#[cfg(windows)]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    fs::remove_dir(path).map_err(|source| SkillReferenceError::io(path, source))
}

#[cfg(not(any(unix, windows)))]
fn remove_reference_unchecked(path: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: path.to_owned(),
    })
}

#[cfg(windows)]
fn is_incomplete_reference(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<bool, SkillReferenceError> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
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
fn is_incomplete_reference(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Result<bool, SkillReferenceError> {
    Ok(false)
}

fn remove_incomplete_private_entry(path: &Path) -> Result<(), SkillReferenceError> {
    fs::remove_dir(path).map_err(|source| SkillReferenceError::io(path, source))?;
    sync_parent(path)
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
))]
fn move_noreplace(source: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE)
        .map_err(|error| SkillReferenceError::io(destination, error.into()))
}

#[cfg(windows)]
fn move_noreplace(source: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    fs::rename(source, destination).map_err(|source| SkillReferenceError::io(destination, source))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    ))
))]
fn move_noreplace(_source: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: destination.to_owned(),
    })
}

#[cfg(not(any(unix, windows)))]
fn move_noreplace(_source: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: destination.to_owned(),
    })
}

fn sync_move_parents(source: &Path, destination: &Path) -> Result<(), SkillReferenceError> {
    sync_parent(source)?;
    if source.parent() != destination.parent() {
        sync_parent(destination)?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), SkillReferenceError> {
    let parent = path.parent().expect("managed reference has a parent");
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
    #[error(
        "native Skill and private state roots are on different filesystems: {native:?}, {state:?}"
    )]
    DifferentFilesystems { native: PathBuf, state: PathBuf },
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
        fs::write(source.join("demo/SKILL.md"), "# Demo\n").unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(&state).unwrap();
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

    #[cfg(any(unix, windows))]
    #[test]
    fn managed_references_are_idempotent_and_reversible() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        let disabled = plan(&source, &destination, &state, false);
        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        assert!(!destination.join("demo").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_unowned_same_source_reference_is_never_adopted() {
        let (_temporary, source, destination, state) = roots();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        create_reference(&target, &destination.join("demo")).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &state, false)),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_replaced_managed_reference_is_quarantined_without_deletion() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        remove_reference_unchecked(&destination.join("demo")).unwrap();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        create_reference(&target, &destination.join("demo")).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &state, false)),
            Err(SkillReferenceError::Conflict { .. }) | Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn ownership_is_bound_to_one_native_root() {
        let (temporary, source, destination_a, state) = roots();
        apply_skill_reference(&plan(&source, &destination_a, &state, true))
            .unwrap()
            .commit()
            .unwrap();
        let destination_b = temporary.path().join("destination-b");
        fs::create_dir(&destination_b).unwrap();
        let target = fs::canonicalize(source.join("demo")).unwrap();
        create_reference(&target, &destination_b.join("demo")).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination_b, &state, false)),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination_b.join("demo/SKILL.md").is_file());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_staged_reference_converges_from_catalog_state() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        park_destination(&paths).unwrap();

        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        park_destination(&paths).unwrap();
        apply_skill_reference(&plan(&source, &destination, &state, false))
            .unwrap()
            .commit()
            .unwrap();
        assert!(!paths.stage.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn source_relocation_updates_the_private_anchor() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();

        let relocated = temporary.path().join("relocated");
        fs::rename(&source, &relocated).unwrap();
        apply_skill_reference(&plan(&relocated, &destination, &state, true))
            .unwrap()
            .commit()
            .unwrap();

        assert!(destination.join("demo/SKILL.md").is_file());
    }
}
