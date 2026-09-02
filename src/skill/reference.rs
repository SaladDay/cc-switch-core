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

const OWNER_VERSION: u8 = 6;
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
    destination_fingerprint: String,
    anchor_fingerprint: String,
    stem: String,
}

impl ReferencePaths {
    fn state_retired_anchor(&self, identity: ObjectIdentity) -> PathBuf {
        self.owner
            .parent()
            .expect("owner path has a parent")
            .join(format!(
                "{}.retired-anchor-{:016x}-{:016x}-{:016x}",
                self.stem, identity.volume, identity.file, identity.generation
            ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    volume: u64,
    file: u64,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingReference {
    name: String,
    identity: ObjectIdentity,
    target: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference_identity: Option<ObjectIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor_identity: Option<ObjectIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_reference: Option<PendingReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_anchor: Option<PendingReference>,
    #[serde(default, skip_serializing_if = "is_false")]
    replacing_anchor: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ReferenceOwner {
    fn initial(plan: &SkillReferencePlan, destination: String, source: String) -> Self {
        Self {
            version: OWNER_VERSION,
            skill_id: plan.skill_id.clone(),
            app: plan.app.as_str().to_owned(),
            directory: plan.directory.clone(),
            destination,
            source,
            pending_source: None,
            reference_identity: None,
            anchor_identity: None,
            pending_reference: None,
            pending_anchor: None,
            replacing_anchor: false,
        }
    }

    fn accepts_source(&self, source: &str) -> bool {
        self.source == source || self.pending_source.as_deref() == Some(source)
    }

    fn pending_anchor_targets(&self, target: &str) -> bool {
        self.pending_source
            .as_deref()
            .map_or(self.source == target, |pending| pending == target)
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
        identity: ObjectIdentity,
    },
    Incomplete,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedLocation {
    Missing,
    Destination,
}

#[derive(Debug, Clone)]
struct ManagedState {
    owner: Option<ReferenceOwner>,
    anchor_target: Option<PathBuf>,
    anchor_source: Option<String>,
    anchor_reachable: bool,
    anchor_identity: Option<ObjectIdentity>,
    location: ManagedLocation,
}

#[derive(Debug)]
struct InspectedAnchor {
    target: Option<PathBuf>,
    source: Option<String>,
    reachable: bool,
    identity: Option<ObjectIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceRecovery {
    FinishAnchorRetirement,
    FinishPendingAnchor,
    FinishPendingReference,
    ForgetMissingReference,
}

#[derive(Debug)]
enum ReferenceInspection {
    Stable(ManagedState),
    Converging(ManagedState),
    Recoverable(ReferenceRecovery),
}

/// Read-only ownership classification used by the snapshot layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillReferenceObservation {
    Missing,
    ManagedPresent,
    ManagedStale,
    ManagedMissing,
    RecoveryPending,
    Unmanaged,
    Conflict,
    Unreadable,
}

pub(super) fn inspect_skill_reference(
    state_root: &Path,
    app: &AppType,
    skill_id: &str,
    directory: &str,
    source_root: &Path,
    expected_source: Option<&Path>,
    destination_root: &Path,
) -> SkillReferenceObservation {
    let plan = SkillReferencePlan::new(
        skill_id,
        app.clone(),
        source_root,
        destination_root,
        state_root,
        directory,
        true,
    );
    let paths = match build_paths(&plan) {
        Ok(paths) => paths,
        Err(_) => return SkillReferenceObservation::Unreadable,
    };
    let expected_source = match expected_source.map(fs::canonicalize).transpose() {
        Ok(source) => source.map(|source| path_fingerprint(&source)),
        Err(_) => return SkillReferenceObservation::Unreadable,
    };
    match inspect_reference_machine(&plan, &paths) {
        Ok(ReferenceInspection::Stable(ManagedState { owner: None, .. })) => {
            SkillReferenceObservation::Missing
        }
        Ok(ReferenceInspection::Stable(state))
            if state.location == ManagedLocation::Destination
                && state.anchor_reachable
                && expected_source
                    .as_deref()
                    .is_some_and(|source| state.anchor_source.as_deref() == Some(source)) =>
        {
            SkillReferenceObservation::ManagedPresent
        }
        Ok(ReferenceInspection::Stable(state))
            if state.location == ManagedLocation::Destination && state.anchor_reachable =>
        {
            SkillReferenceObservation::ManagedStale
        }
        Ok(ReferenceInspection::Stable(_)) => SkillReferenceObservation::ManagedMissing,
        Ok(ReferenceInspection::Converging(_)) => SkillReferenceObservation::RecoveryPending,
        Ok(ReferenceInspection::Recoverable(_)) => SkillReferenceObservation::RecoveryPending,
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
    target_source: Option<String>,
}

impl SkillReferenceReceipt {
    pub fn verify(&self) -> Result<(), SkillReferenceError> {
        let state = managed_state(&self.plan, &self.paths)?;
        if self.plan.enabled {
            let target_source =
                self.target_source
                    .as_deref()
                    .ok_or_else(|| SkillReferenceError::Conflict {
                        path: self.paths.owner.clone(),
                    })?;
            let owner = state
                .owner
                .as_ref()
                .ok_or_else(|| SkillReferenceError::Conflict {
                    path: self.paths.owner.clone(),
                })?;
            if state.location != ManagedLocation::Destination
                || state.anchor_source.as_deref() != Some(target_source)
                || !state.anchor_reachable
                || owner.source != target_source
                || owner.pending_source.is_some()
                || owner.reference_identity.is_none()
                || owner.anchor_identity.is_none()
                || owner.replacing_anchor
            {
                return Err(SkillReferenceError::Conflict {
                    path: self.paths.destination.clone(),
                });
            }
        } else if state.anchor_reachable
            || state.owner.as_ref().is_some_and(|owner| {
                owner.anchor_identity.is_some()
                    || owner.pending_source.is_some()
                    || owner.pending_reference.is_some()
                    || owner.pending_anchor.is_some()
                    || owner.replacing_anchor
            })
        {
            return Err(SkillReferenceError::Conflict {
                path: self.paths.destination.clone(),
            });
        }
        Ok(())
    }

    pub fn commit(self) -> Result<(), SkillReferenceError> {
        self.verify()
    }

    /// Restores the previous native visibility. Private recovery records may
    /// remain so Core never has to delete an object whose ownership changed.
    pub fn rollback(self) -> Result<(), SkillReferenceError> {
        restore_previous(&self.paths, &self.plan, &self.previous)
    }
}

/// Applies one idempotent, owner-checked native reference intent.
///
/// The host supplies a private state root on the same filesystem as the
/// native root and creates both roots while holding the shared live-config
/// lock. The state root is part of the persistent reference identity and must
/// remain identical across cooperating hosts; relocating it requires an
/// explicit offline migration. Core creates references only in that private
/// root, then atomically moves the public entry into place without replacement.
/// Later switches leave that entry untouched and change only its private
/// anchor. Disabling makes the public entry resolve to no Skill without
/// deleting or moving it, and does not require the current source to exist.
pub fn apply_skill_reference(
    plan: &SkillReferencePlan,
) -> Result<SkillReferenceReceipt, SkillReferenceError> {
    let paths = prepare_paths(plan)?;
    let target = if plan.enabled {
        let source = fs::canonicalize(&paths.source)
            .map_err(|error| SkillReferenceError::io(&paths.source, error))?;
        let fingerprint = path_fingerprint(&source);
        Some((source, fingerprint))
    } else {
        None
    };
    recover_reference_state(plan, &paths)?;
    let previous = managed_state(plan, &paths)?;

    let applied = match target.as_ref() {
        Some((source, fingerprint)) => converge_enabled(plan, &paths, source, fingerprint),
        None => converge_disabled(plan, &paths),
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
        target_source: target.map(|(_, fingerprint)| fingerprint),
    })
}

fn prepare_paths(plan: &SkillReferencePlan) -> Result<ReferencePaths, SkillReferenceError> {
    for root in [&plan.source_root, &plan.destination_root, &plan.state_root] {
        if !root.is_absolute() {
            return Err(SkillReferenceError::RelativeRoot { path: root.clone() });
        }
    }
    let paths = build_paths(plan)?;
    if plan.enabled {
        validate_source(&paths.source)?;
    }
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    require_real_directory(&plan.destination_root)?;
    require_private_state_directory(&plan.state_root)?;
    require_same_filesystem(&plan.destination_root, &plan.state_root)?;
    require_stable_identity(&plan.state_root)?;
    validate_root_pair(&plan.source_root, &plan.destination_root)?;
    validate_root_pair(&plan.source_root, &plan.state_root)?;
    validate_root_pair(&plan.destination_root, &plan.state_root)?;
    Ok(paths)
}

#[cfg(unix)]
fn require_stable_identity(path: &Path) -> Result<(), SkillReferenceError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| SkillReferenceError::io(path, source))?;
    object_generation(path, &metadata).map(|_| ())
}

#[cfg(not(unix))]
fn require_stable_identity(_path: &Path) -> Result<(), SkillReferenceError> {
    Ok(())
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
        destination_fingerprint,
        anchor_fingerprint,
        stem,
    })
}

fn managed_state(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<ManagedState, SkillReferenceError> {
    match inspect_reference_machine(plan, paths)? {
        ReferenceInspection::Stable(state) | ReferenceInspection::Converging(state) => Ok(state),
        ReferenceInspection::Recoverable(_) => Err(SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        }),
    }
}

fn inspect_reference_machine(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<ReferenceInspection, SkillReferenceError> {
    let owner = read_owner(&paths.owner, plan, &paths.destination_fingerprint)?;
    let destination = inspect_reference(&paths.destination)?;
    let anchor = inspect_reference(&paths.anchor)?;

    let Some(owner) = owner else {
        if matches!(destination, ReferenceEntry::Missing)
            && matches!(anchor, ReferenceEntry::Missing)
        {
            return Ok(ReferenceInspection::Stable(ManagedState {
                owner: None,
                anchor_target: None,
                anchor_source: None,
                anchor_reachable: false,
                anchor_identity: None,
                location: ManagedLocation::Missing,
            }));
        }
        return Err(SkillReferenceError::Unowned {
            path: first_present_path(paths, &destination, &anchor),
        });
    };
    if let Some(pending) = owner.pending_anchor.as_ref() {
        inspect_owned_reference_location(&owner, paths, &destination)?;
        pending_publication_location(paths, pending, &paths.anchor)?;
        return Ok(ReferenceInspection::Recoverable(
            ReferenceRecovery::FinishPendingAnchor,
        ));
    }
    if let Some(pending) = owner.pending_reference.as_ref() {
        inspect_owned_anchor(&owner, paths, &anchor)?;
        if pending.target != paths.anchor_fingerprint {
            return Err(SkillReferenceError::InvalidOwner {
                path: paths.owner.clone(),
            });
        }
        pending_publication_location(paths, pending, &paths.destination)?;
        return Ok(ReferenceInspection::Recoverable(
            ReferenceRecovery::FinishPendingReference,
        ));
    }
    if owner.replacing_anchor && owner.anchor_identity.is_some() {
        inspect_owned_reference_location(&owner, paths, &destination)?;
        inspect_anchor_retirement(paths, &owner, &anchor)?;
        return Ok(ReferenceInspection::Recoverable(
            ReferenceRecovery::FinishAnchorRetirement,
        ));
    }

    let (location, reference_identity) =
        inspect_owned_reference_location(&owner, paths, &destination)?;
    if owner.reference_identity.is_some() && reference_identity.is_none() {
        return Ok(ReferenceInspection::Recoverable(
            ReferenceRecovery::ForgetMissingReference,
        ));
    }

    let anchor_state = inspect_owned_anchor(&owner, paths, &anchor)?;
    let converging = owner.pending_source.is_some() || owner.replacing_anchor;
    let state = ManagedState {
        owner: Some(owner),
        anchor_target: anchor_state.target,
        anchor_source: anchor_state.source,
        anchor_reachable: anchor_state.reachable,
        anchor_identity: anchor_state.identity,
        location,
    };
    Ok(if converging {
        ReferenceInspection::Converging(state)
    } else {
        ReferenceInspection::Stable(state)
    })
}

fn inspect_owned_reference_location(
    owner: &ReferenceOwner,
    paths: &ReferencePaths,
    destination: &ReferenceEntry,
) -> Result<(ManagedLocation, Option<ObjectIdentity>), SkillReferenceError> {
    let destination_present = require_direct_anchor(
        destination,
        paths,
        &paths.destination,
        owner.reference_identity,
    )?;
    if owner.reference_identity.is_none() && destination_present.is_some() {
        return Err(SkillReferenceError::Unowned {
            path: paths.destination.clone(),
        });
    }
    let location = if destination_present.is_some() {
        ManagedLocation::Destination
    } else {
        ManagedLocation::Missing
    };
    Ok((location, destination_present))
}

fn inspect_owned_anchor(
    owner: &ReferenceOwner,
    paths: &ReferencePaths,
    anchor: &ReferenceEntry,
) -> Result<InspectedAnchor, SkillReferenceError> {
    let state = match anchor {
        ReferenceEntry::Missing => InspectedAnchor {
            target: None,
            source: None,
            reachable: false,
            identity: None,
        },
        ReferenceEntry::Reference {
            declared_target,
            resolved_fingerprint,
            reachable,
            identity,
            ..
        } if owner.accepts_source(resolved_fingerprint)
            && owner
                .anchor_identity
                .is_none_or(|expected| expected == *identity) =>
        {
            InspectedAnchor {
                target: Some(declared_target.clone()),
                source: Some(resolved_fingerprint.clone()),
                reachable: *reachable,
                identity: Some(*identity),
            }
        }
        _ => {
            return Err(SkillReferenceError::Conflict {
                path: paths.anchor.clone(),
            })
        }
    };
    if owner.anchor_identity.is_none() && state.identity.is_some() {
        return Err(SkillReferenceError::Unowned {
            path: paths.anchor.clone(),
        });
    }
    Ok(state)
}

fn first_present_path(
    paths: &ReferencePaths,
    destination: &ReferenceEntry,
    anchor: &ReferenceEntry,
) -> PathBuf {
    if !matches!(destination, ReferenceEntry::Missing) {
        paths.destination.clone()
    } else {
        debug_assert!(!matches!(anchor, ReferenceEntry::Missing));
        paths.anchor.clone()
    }
}

fn require_direct_anchor(
    entry: &ReferenceEntry,
    paths: &ReferencePaths,
    path: &Path,
    expected_identity: Option<ObjectIdentity>,
) -> Result<Option<ObjectIdentity>, SkillReferenceError> {
    match entry {
        ReferenceEntry::Missing => Ok(None),
        ReferenceEntry::Reference {
            declared_fingerprint,
            identity,
            ..
        } if declared_fingerprint == &paths.anchor_fingerprint
            && expected_identity.is_none_or(|expected| expected == *identity) =>
        {
            Ok(Some(*identity))
        }
        _ => Err(SkillReferenceError::Unowned {
            path: path.to_owned(),
        }),
    }
}

fn recover_reference_state(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<(), SkillReferenceError> {
    loop {
        let recovery = match inspect_reference_machine(plan, paths)? {
            ReferenceInspection::Stable(_) | ReferenceInspection::Converging(_) => return Ok(()),
            ReferenceInspection::Recoverable(recovery) => recovery,
        };
        let mut owner = read_owner(&paths.owner, plan, &paths.destination_fingerprint)?
            .ok_or_else(|| SkillReferenceError::Conflict {
                path: paths.owner.clone(),
            })?;
        match recovery {
            ReferenceRecovery::FinishAnchorRetirement => {
                finish_anchor_retirement(paths, &mut owner)?
            }
            ReferenceRecovery::FinishPendingAnchor => finish_pending_anchor(paths, &mut owner)?,
            ReferenceRecovery::FinishPendingReference => {
                finish_pending_reference(paths, &mut owner)?
            }
            ReferenceRecovery::ForgetMissingReference => {
                if owner.reference_identity.is_none()
                    || owner.pending_reference.is_some()
                    || owner.pending_anchor.is_some()
                {
                    return Err(SkillReferenceError::InvalidOwner {
                        path: paths.owner.clone(),
                    });
                }
                owner.reference_identity = None;
                write_owner(&paths.owner, &owner)?;
            }
        }
    }
}

fn finish_pending_anchor(
    paths: &ReferencePaths,
    owner: &mut ReferenceOwner,
) -> Result<(), SkillReferenceError> {
    let pending = owner
        .pending_anchor
        .clone()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    if !owner.replacing_anchor
        || owner.anchor_identity.is_some()
        || !owner.pending_anchor_targets(&pending.target)
    {
        return Err(SkillReferenceError::InvalidOwner {
            path: paths.owner.clone(),
        });
    }
    publish_pending_reference(paths, &pending, &paths.anchor)?;
    owner.source = pending.target;
    owner.pending_source = None;
    owner.anchor_identity = Some(pending.identity);
    owner.pending_anchor = None;
    owner.replacing_anchor = false;
    write_owner(&paths.owner, owner)
}

fn finish_anchor_retirement(
    paths: &ReferencePaths,
    owner: &mut ReferenceOwner,
) -> Result<(), SkillReferenceError> {
    let identity = owner
        .anchor_identity
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    if !owner.replacing_anchor || owner.pending_anchor.is_some() {
        return Err(SkillReferenceError::InvalidOwner {
            path: paths.owner.clone(),
        });
    }
    let anchor = inspect_reference(&paths.anchor)?;
    let retired = paths.state_retired_anchor(identity);
    match inspect_anchor_retirement(paths, owner, &anchor)? {
        AnchorRetirementLocation::Anchor => {
            move_noreplace(&paths.anchor, &retired)?;
            sync_move_parents(&paths.anchor, &retired)?;
        }
        AnchorRetirementLocation::Retired => {}
    }
    if !anchor_matches_owner(&inspect_reference(&retired)?, identity, owner) {
        return Err(SkillReferenceError::Unowned { path: retired });
    }
    owner.anchor_identity = None;
    owner.replacing_anchor = false;
    write_owner(&paths.owner, owner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorRetirementLocation {
    Anchor,
    Retired,
}

fn inspect_anchor_retirement(
    paths: &ReferencePaths,
    owner: &ReferenceOwner,
    anchor: &ReferenceEntry,
) -> Result<AnchorRetirementLocation, SkillReferenceError> {
    let identity = owner
        .anchor_identity
        .ok_or_else(|| SkillReferenceError::InvalidOwner {
            path: paths.owner.clone(),
        })?;
    if !owner.replacing_anchor || owner.pending_anchor.is_some() {
        return Err(SkillReferenceError::InvalidOwner {
            path: paths.owner.clone(),
        });
    }
    let retired = paths.state_retired_anchor(identity);
    let parked = inspect_reference(&retired)?;
    match (anchor, &parked) {
        (entry, ReferenceEntry::Missing) if anchor_matches_owner(entry, identity, owner) => {
            Ok(AnchorRetirementLocation::Anchor)
        }
        (ReferenceEntry::Missing, entry) if anchor_matches_owner(entry, identity, owner) => {
            Ok(AnchorRetirementLocation::Retired)
        }
        (ReferenceEntry::Missing, ReferenceEntry::Missing) => Err(SkillReferenceError::Conflict {
            path: paths.anchor.clone(),
        }),
        (ReferenceEntry::Missing, _) => Err(SkillReferenceError::Unowned { path: retired }),
        (_, ReferenceEntry::Missing) => Err(SkillReferenceError::Unowned {
            path: paths.anchor.clone(),
        }),
        _ => Err(SkillReferenceError::Conflict {
            path: paths.anchor.clone(),
        }),
    }
}

fn anchor_matches_owner(
    entry: &ReferenceEntry,
    identity: ObjectIdentity,
    owner: &ReferenceOwner,
) -> bool {
    matches!(
        entry,
        ReferenceEntry::Reference {
            identity: actual,
            resolved_fingerprint,
            ..
        } if *actual == identity && resolved_fingerprint == &owner.source
    )
}

fn finish_pending_reference(
    paths: &ReferencePaths,
    owner: &mut ReferenceOwner,
) -> Result<(), SkillReferenceError> {
    let pending = owner
        .pending_reference
        .clone()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    if owner.replacing_anchor
        || owner.anchor_identity.is_none()
        || owner.reference_identity.is_some()
        || pending.target != paths.anchor_fingerprint
    {
        return Err(SkillReferenceError::InvalidOwner {
            path: paths.owner.clone(),
        });
    }
    publish_pending_reference(paths, &pending, &paths.destination)?;
    owner.reference_identity = Some(pending.identity);
    owner.pending_reference = None;
    write_owner(&paths.owner, owner)
}

fn publish_pending_reference(
    paths: &ReferencePaths,
    pending: &PendingReference,
    destination: &Path,
) -> Result<(), SkillReferenceError> {
    let source = pending_reference_path(paths, pending)?;
    match pending_publication_location(paths, pending, destination)? {
        PendingPublicationLocation::Private => {
            move_noreplace(&source, destination)?;
            sync_move_parents(&source, destination)?;
        }
        PendingPublicationLocation::Published => {}
    }
    if pending_reference_matches(&inspect_reference(destination)?, pending) {
        Ok(())
    } else {
        Err(SkillReferenceError::Unowned {
            path: destination.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPublicationLocation {
    Private,
    Published,
}

fn pending_reference_path(
    paths: &ReferencePaths,
    pending: &PendingReference,
) -> Result<PathBuf, SkillReferenceError> {
    let parent = paths
        .owner
        .parent()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    Ok(parent.join(&pending.name))
}

fn pending_publication_location(
    paths: &ReferencePaths,
    pending: &PendingReference,
    destination: &Path,
) -> Result<PendingPublicationLocation, SkillReferenceError> {
    let source = pending_reference_path(paths, pending)?;
    let source_entry = inspect_reference(&source)?;
    let destination_entry = inspect_reference(destination)?;
    match (&source_entry, &destination_entry) {
        (entry, ReferenceEntry::Missing) if pending_reference_matches(entry, pending) => {
            Ok(PendingPublicationLocation::Private)
        }
        (ReferenceEntry::Missing, entry) if pending_reference_matches(entry, pending) => {
            Ok(PendingPublicationLocation::Published)
        }
        (ReferenceEntry::Missing, ReferenceEntry::Missing) => {
            Err(SkillReferenceError::Conflict { path: source })
        }
        (ReferenceEntry::Missing, _) => Err(SkillReferenceError::Unowned {
            path: destination.to_owned(),
        }),
        (_, ReferenceEntry::Missing) => Err(SkillReferenceError::Unowned { path: source }),
        _ => Err(SkillReferenceError::Conflict {
            path: destination.to_owned(),
        }),
    }
}

fn pending_reference_matches(entry: &ReferenceEntry, pending: &PendingReference) -> bool {
    matches!(
        entry,
        ReferenceEntry::Reference {
            identity,
            declared_fingerprint,
            ..
        } if *identity == pending.identity && declared_fingerprint == &pending.target
    )
}

fn converge_enabled(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
    source: &Path,
    source_fingerprint: &str,
) -> Result<(), SkillReferenceError> {
    let mut state = managed_state(plan, paths)?;
    let mut owner = match state.owner.clone() {
        Some(owner) => {
            let working = owner.transitioning_to(source_fingerprint);
            if working != owner {
                write_owner(&paths.owner, &working)?;
            }
            working
        }
        None => {
            let owner = ReferenceOwner::initial(
                plan,
                paths.destination_fingerprint.clone(),
                source_fingerprint.to_owned(),
            );
            publish_owner(&paths.owner, &owner)?;
            owner
        }
    };

    ensure_anchor(paths, source, source_fingerprint, &mut owner, &mut state)?;

    match state.location {
        ManagedLocation::Destination => {}
        ManagedLocation::Missing => {
            owner.pending_reference = Some(create_pending_reference(&paths.anchor, paths)?);
            write_owner(&paths.owner, &owner)?;
            finish_pending_reference(paths, &mut owner)?;
        }
    }
    if owner.source != source_fingerprint
        || owner.pending_source.is_some()
        || owner.reference_identity.is_none()
        || owner.anchor_identity.is_none()
        || owner.replacing_anchor
    {
        return Err(SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        });
    }
    Ok(())
}

fn converge_disabled(
    plan: &SkillReferencePlan,
    paths: &ReferencePaths,
) -> Result<(), SkillReferenceError> {
    let state = managed_state(plan, paths)?;
    if let Some(mut owner) = state.owner.clone() {
        if owner.pending_source.is_some() {
            owner.pending_source = None;
            write_owner(&paths.owner, &owner)?;
        }
        if owner.replacing_anchor && owner.anchor_identity.is_none() {
            owner.replacing_anchor = false;
            write_owner(&paths.owner, &owner)?;
        }
        if owner.anchor_identity.is_some() {
            owner.replacing_anchor = true;
            write_owner(&paths.owner, &owner)?;
            finish_anchor_retirement(paths, &mut owner)?;
        }
    }
    let state = managed_state(plan, paths)?;
    if state.owner.as_ref().is_some_and(|owner| {
        owner.pending_source.is_some()
            || owner.pending_reference.is_some()
            || owner.pending_anchor.is_some()
            || owner.replacing_anchor
    }) {
        Err(SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })
    } else {
        Ok(())
    }
}

fn restore_previous(
    paths: &ReferencePaths,
    plan: &SkillReferencePlan,
    previous: &ManagedState,
) -> Result<(), SkillReferenceError> {
    let previous_enabled =
        if previous.location == ManagedLocation::Destination && previous.anchor_reachable {
            let source =
                previous
                    .anchor_target
                    .as_deref()
                    .ok_or_else(|| SkillReferenceError::Conflict {
                        path: paths.anchor.clone(),
                    })?;
            let fingerprint =
                previous
                    .anchor_source
                    .as_deref()
                    .ok_or_else(|| SkillReferenceError::Conflict {
                        path: paths.anchor.clone(),
                    })?;
            validate_source(source)?;
            let source =
                fs::canonicalize(source).map_err(|error| SkillReferenceError::io(source, error))?;
            if path_fingerprint(&source) != fingerprint {
                return Err(SkillReferenceError::Conflict { path: source });
            }
            Some((source, fingerprint.to_owned()))
        } else {
            None
        };

    recover_reference_state(plan, paths)?;
    if let Some((source, fingerprint)) = previous_enabled {
        converge_enabled(plan, paths, &source, &fingerprint)
    } else {
        converge_disabled(plan, paths)
    }
}

fn ensure_anchor(
    paths: &ReferencePaths,
    source: &Path,
    source_fingerprint: &str,
    owner: &mut ReferenceOwner,
    state: &mut ManagedState,
) -> Result<(), SkillReferenceError> {
    if state.anchor_source.as_deref() == Some(source_fingerprint) && state.anchor_reachable {
        let identity = state
            .anchor_identity
            .ok_or_else(|| SkillReferenceError::Conflict {
                path: paths.anchor.clone(),
            })?;
        if owner.anchor_identity != Some(identity)
            || owner.replacing_anchor
            || owner.pending_source.is_some()
        {
            owner.source = source_fingerprint.to_owned();
            owner.pending_source = None;
            owner.anchor_identity = Some(identity);
            owner.replacing_anchor = false;
            write_owner(&paths.owner, owner)?;
        }
        return Ok(());
    }

    owner.replacing_anchor = true;
    if owner.source != source_fingerprint {
        owner.pending_source = Some(source_fingerprint.to_owned());
    }
    write_owner(&paths.owner, owner)?;

    if owner.anchor_identity.is_some() {
        finish_anchor_retirement(paths, owner)?;
    } else {
        require_missing(&paths.anchor)?;
    }
    owner.replacing_anchor = true;
    owner.pending_anchor = Some(create_pending_reference(source, paths)?);
    write_owner(&paths.owner, owner)?;
    finish_pending_anchor(paths, owner)?;
    let identity = owner
        .anchor_identity
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    state.anchor_target = Some(source.to_owned());
    state.anchor_source = Some(source_fingerprint.to_owned());
    state.anchor_reachable = true;
    state.anchor_identity = Some(identity);
    Ok(())
}

fn create_pending_reference(
    target: &Path,
    paths: &ReferencePaths,
) -> Result<PendingReference, SkillReferenceError> {
    let parent = paths
        .owner
        .parent()
        .ok_or_else(|| SkillReferenceError::Conflict {
            path: paths.owner.clone(),
        })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = OWNER_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        ".cc-switch-reference.{}.{timestamp}.{counter}",
        std::process::id()
    );
    let temporary = parent.join(&name);
    create_reference(target, &temporary)?;
    let (identity, declared_fingerprint) = match inspect_reference(&temporary)? {
        ReferenceEntry::Reference {
            identity,
            declared_fingerprint,
            ..
        } => (identity, declared_fingerprint),
        _ => {
            return Err(SkillReferenceError::Conflict { path: temporary });
        }
    };
    Ok(PendingReference {
        name,
        identity,
        target: declared_fingerprint,
    })
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

fn require_private_state_directory(path: &Path) -> Result<(), SkillReferenceError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| SkillReferenceError::io(path, source))?;
    if !metadata.file_type().is_dir() {
        return Err(SkillReferenceError::InvalidRoot {
            path: path.to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(SkillReferenceError::InsecureStateRoot {
                path: path.to_owned(),
            });
        }
    }
    Ok(())
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
    let identity = object_identity(path, &metadata)?;
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
        identity,
    })
}

#[cfg(unix)]
fn object_identity(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<ObjectIdentity, SkillReferenceError> {
    use std::os::unix::fs::MetadataExt;

    Ok(ObjectIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
        generation: object_generation(path, metadata)?,
    })
}

#[cfg(unix)]
fn object_generation(path: &Path, metadata: &fs::Metadata) -> Result<u64, SkillReferenceError> {
    creation_generation(path, metadata.created())
}

#[cfg(unix)]
fn creation_generation(
    path: &Path,
    created: std::io::Result<SystemTime>,
) -> Result<u64, SkillReferenceError> {
    let created = match created {
        Ok(created) => created,
        Err(source) if source.kind() == std::io::ErrorKind::Unsupported => {
            return Err(SkillReferenceError::UnsupportedFilesystem {
                path: path.to_owned(),
            })
        }
        Err(source) => return Err(SkillReferenceError::io(path, source)),
    };
    let (side, duration) = match created.duration_since(UNIX_EPOCH) {
        Ok(duration) => (1_u8, duration),
        Err(error) => (0_u8, error.duration()),
    };
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-object-generation\0");
    hasher.update([side]);
    hasher.update(duration.as_secs().to_le_bytes());
    hasher.update(duration.subsec_nanos().to_le_bytes());
    let digest = hasher.finalize();
    Ok(u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix has eight bytes"),
    ))
}

#[cfg(windows)]
fn object_identity(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Result<ObjectIdentity, SkillReferenceError> {
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(SkillReferenceError::io(
            path,
            std::io::Error::last_os_error(),
        ));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut information) };
    if result == 0 {
        return Err(SkillReferenceError::io(
            path,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(ObjectIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        generation: (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(information.ftCreationTime.dwLowDateTime),
    })
}

#[cfg(not(any(unix, windows)))]
fn object_identity(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Result<ObjectIdentity, SkillReferenceError> {
    Err(SkillReferenceError::UnsupportedPlatform {
        path: path.to_owned(),
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
    hasher.update(b"cc-switch-skill-reference-v6\0");
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
        || owner.skill_id != plan.skill_id
        || owner.app != plan.app.as_str()
        || owner.directory != plan.directory
        || owner.destination != destination_fingerprint
        || !valid_owner_structure(&owner)
    {
        return Err(SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        });
    }
    Ok(Some(owner))
}

fn valid_owner_structure(owner: &ReferenceOwner) -> bool {
    let valid_identity_state = match (&owner.pending_reference, &owner.pending_anchor) {
        (None, None) => matches!(
            (
                owner.reference_identity,
                owner.anchor_identity,
                owner.replacing_anchor,
            ),
            (None, None, false)
                | (None, None, true)
                | (None, Some(_), false)
                | (None, Some(_), true)
                | (Some(_), None, false)
                | (Some(_), None, true)
                | (Some(_), Some(_), false)
                | (Some(_), Some(_), true)
        ),
        (Some(pending), None) => {
            owner.reference_identity.is_none()
                && owner.anchor_identity.is_some()
                && !owner.replacing_anchor
                && valid_pending_reference(pending)
        }
        (None, Some(pending)) => {
            owner.anchor_identity.is_none()
                && owner.replacing_anchor
                && owner.pending_anchor_targets(&pending.target)
                && valid_pending_reference(pending)
        }
        (Some(_), Some(_)) => false,
    };
    owner.version == OWNER_VERSION
        && valid_digest(&owner.destination)
        && valid_digest(&owner.source)
        && !owner
            .pending_source
            .as_deref()
            .is_some_and(|source| !valid_digest(source) || source == owner.source)
        && valid_identity_state
}

fn valid_pending_reference(pending: &PendingReference) -> bool {
    let mut components = Path::new(&pending.name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(name)), None)
            if name.to_str() == Some(pending.name.as_str())
    ) && pending.name.starts_with(".cc-switch-reference.")
        && pending.name.len() <= 255
        && !pending.name.chars().any(char::is_control)
        && valid_digest(&pending.target)
}

fn owner_bytes(owner: &ReferenceOwner, path: &Path) -> Result<Vec<u8>, SkillReferenceError> {
    if !valid_owner_structure(owner) {
        return Err(SkillReferenceError::InvalidOwner {
            path: path.to_owned(),
        });
    }
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
    move_noreplace(&temporary, path)?;
    sync_move_parents(&temporary, path)
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
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let wide_path = |path: &Path| -> Result<Vec<u16>, SkillReferenceError> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(SkillReferenceError::io(
                path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains an embedded NUL",
                ),
            ));
        }
        wide.push(0);
        Ok(wide)
    };
    let source_wide = wide_path(source)?;
    let destination_wide = wide_path(destination)?;
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), 0) };
    if result == 0 {
        Err(SkillReferenceError::io(
            destination,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
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
    #[error("private Skill state directory is accessible to other users: {path:?}")]
    InsecureStateRoot { path: PathBuf },
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
    #[error("Skill references require filesystem creation-time identity: {path:?}")]
    UnsupportedFilesystem { path: PathBuf },
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

    #[cfg(unix)]
    #[test]
    fn unsupported_creation_time_is_rejected_explicitly() {
        let path = Path::new("/state");
        let error = creation_generation(
            path,
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SkillReferenceError::UnsupportedFilesystem { path: actual } if actual == path
        ));
    }

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        let state = temporary.path().join("state");
        fs::create_dir_all(source.join("demo")).unwrap();
        fs::write(source.join("demo/SKILL.md"), "# Demo\n").unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(&state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (temporary, source, destination, state)
    }

    #[cfg(unix)]
    fn delete_reference_for_test(path: &Path) {
        fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    fn delete_reference_for_test(path: &Path) {
        fs::remove_dir(path).unwrap();
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

    fn catalog_entry(selected: bool) -> crate::SkillCatalogEntry {
        catalog_entry_for("enabled_claude", selected)
    }

    fn catalog_entry_for(column_name: &str, selected: bool) -> crate::SkillCatalogEntry {
        crate::SkillCatalogEntry::try_new(
            "owner/repo:demo",
            "demo",
            None,
            "demo",
            crate::skill_catalog_columns()
                .map(|column| (column, selected && column.as_str() == column_name)),
        )
        .unwrap()
    }

    fn reconciliation_reference_plan(
        source: &Path,
        destination: &Path,
        state: &Path,
    ) -> SkillReferencePlan {
        reconciliation_reference_plan_for(source, destination, state, true)
    }

    fn reconciliation_reference_plan_for(
        source: &Path,
        destination: &Path,
        state: &Path,
        selected: bool,
    ) -> SkillReferencePlan {
        let runtime = crate::SkillRuntime::try_new(
            source,
            source.parent().unwrap().join("unified"),
            [crate::SkillAppRuntime::try_new(AppType::Claude, destination, state, None).unwrap()],
        )
        .unwrap();
        let catalog = [catalog_entry(selected)];
        let snapshot = crate::inspect_installed_skills(&catalog, &runtime).unwrap();
        let app = snapshot[0].apps().next().unwrap();
        assert_eq!(
            app.reason(),
            Some(crate::SkillControlReason::RecoveryPending)
        );
        assert!(matches!(
            crate::prepare_skill_switch(
                &catalog,
                "owner/repo:demo",
                &runtime,
                &AppType::Claude,
                selected,
            ),
            Err(crate::SkillPrepareError::Unavailable {
                reason: Some(crate::SkillControlReason::RecoveryPending),
                ..
            })
        ));
        crate::prepare_skill_reconciliation(&catalog, "owner/repo:demo", &runtime, &AppType::Claude)
            .unwrap()
            .reference()
            .unwrap()
            .clone()
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
        windows,
    ))]
    #[test]
    fn no_replace_move_preserves_an_existing_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::write(&source, "source").unwrap();
        fs::write(&destination, "destination").unwrap();

        assert!(move_noreplace(&source, &destination).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "destination");
    }

    #[cfg(windows)]
    #[test]
    fn no_replace_move_preserves_an_existing_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&target).unwrap();
        create_reference(&target, &source).unwrap();
        fs::create_dir(&destination).unwrap();

        assert!(move_noreplace(&source, &destination).is_err());
        assert!(matches!(
            inspect_reference(&source).unwrap(),
            ReferenceEntry::Reference { .. }
        ));
        assert!(matches!(
            inspect_reference(&destination).unwrap(),
            ReferenceEntry::Incomplete | ReferenceEntry::Other
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn managed_references_are_idempotent_and_reversible() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());
        let identity = match inspect_reference(&destination.join("demo")).unwrap() {
            ReferenceEntry::Reference { identity, .. } => identity,
            _ => panic!("managed destination must be a reference"),
        };

        let disabled = plan(&source, &destination, &state, false);
        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        apply_skill_reference(&disabled).unwrap().commit().unwrap();
        assert!(!destination.join("demo/SKILL.md").exists());
        assert!(matches!(
            inspect_reference(&destination.join("demo")).unwrap(),
            ReferenceEntry::Reference {
                identity: current,
                reachable: false,
                ..
            } if current == identity
        ));

        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());
        assert_eq!(
            match inspect_reference(&destination.join("demo")).unwrap() {
                ReferenceEntry::Reference { identity, .. } => identity,
                _ => panic!("re-enabled destination must be a reference"),
            },
            identity
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_root_must_be_private_before_writes() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, source, destination, state) = roots();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &state, true)),
            Err(SkillReferenceError::InsecureStateRoot { .. })
        ));
        assert!(!destination.join("demo").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn changing_the_persistent_state_root_never_adopts_a_public_reference() {
        let (temporary, source, destination, state) = roots();
        apply_skill_reference(&plan(&source, &destination, &state, true))
            .unwrap()
            .commit()
            .unwrap();
        let relocated_state = temporary.path().join("relocated-state");
        fs::create_dir(&relocated_state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&relocated_state, fs::Permissions::from_mode(0o700)).unwrap();
        }

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &relocated_state, true)),
            Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
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
    fn an_exact_target_replacement_is_refused_without_deletion() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        delete_reference_for_test(&destination.join("demo"));
        create_reference(&paths.anchor, &destination.join("demo")).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &state, false)),
            Err(SkillReferenceError::Conflict { .. }) | Err(SkillReferenceError::Unowned { .. })
        ));
        assert!(destination.join("demo/SKILL.md").is_file());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_unbound_anchor_checkpoint_is_never_adopted() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(&paths.source).unwrap();
        let owner = ReferenceOwner::initial(
            &enabled,
            paths.destination_fingerprint.clone(),
            path_fingerprint(&target),
        );
        publish_owner(&paths.owner, &owner).unwrap();
        create_reference(&target, &paths.anchor).unwrap();

        assert!(matches!(
            apply_skill_reference(&plan(&source, &destination, &state, false)),
            Err(SkillReferenceError::Unowned { .. })
        ));
        delete_reference_for_test(&paths.anchor);
        create_reference(&target, &paths.anchor).unwrap();
        assert!(matches!(
            apply_skill_reference(&enabled),
            Err(SkillReferenceError::Unowned { .. })
        ));
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
    fn an_anchor_checkpoint_converges_after_interruption() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(&paths.source).unwrap();
        let source_fingerprint = path_fingerprint(&target);
        let mut owner = ReferenceOwner::initial(
            &enabled,
            paths.destination_fingerprint.clone(),
            source_fingerprint,
        );
        publish_owner(&paths.owner, &owner).unwrap();
        owner.replacing_anchor = true;
        owner.pending_anchor = Some(create_pending_reference(&target, &paths).unwrap());
        write_owner(&paths.owner, &owner).unwrap();

        let recovery = reconciliation_reference_plan(&source, &destination, &state);
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        assert!(destination.join("demo/SKILL.md").is_file());
        assert!(managed_state(&enabled, &paths)
            .unwrap()
            .owner
            .unwrap()
            .reference_identity
            .is_some());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_published_anchor_checkpoint_converges_after_interruption() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(&paths.source).unwrap();
        let mut owner = ReferenceOwner::initial(
            &enabled,
            paths.destination_fingerprint.clone(),
            path_fingerprint(&target),
        );
        owner.replacing_anchor = true;
        owner.pending_anchor = Some(create_pending_reference(&target, &paths).unwrap());
        publish_owner(&paths.owner, &owner).unwrap();
        publish_pending_reference(
            &paths,
            owner.pending_anchor.as_ref().unwrap(),
            &paths.anchor,
        )
        .unwrap();

        let recovery = reconciliation_reference_plan(&source, &destination, &state);
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        let recovered = managed_state(&enabled, &paths).unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert!(recovered.owner.unwrap().pending_anchor.is_none());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_pending_public_reference_converges_after_interruption() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(&paths.source).unwrap();
        let mut owner = ReferenceOwner::initial(
            &enabled,
            paths.destination_fingerprint.clone(),
            path_fingerprint(&target),
        );
        owner.replacing_anchor = true;
        owner.pending_anchor = Some(create_pending_reference(&target, &paths).unwrap());
        publish_owner(&paths.owner, &owner).unwrap();
        finish_pending_anchor(&paths, &mut owner).unwrap();
        owner.pending_reference = Some(create_pending_reference(&paths.anchor, &paths).unwrap());
        write_owner(&paths.owner, &owner).unwrap();

        let recovery = reconciliation_reference_plan(&source, &destination, &state);
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        let recovered = managed_state(&enabled, &paths).unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert!(recovered.owner.unwrap().pending_reference.is_none());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_published_public_reference_converges_through_reconciliation() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        let paths = prepare_paths(&enabled).unwrap();
        let target = fs::canonicalize(&paths.source).unwrap();
        let mut owner = ReferenceOwner::initial(
            &enabled,
            paths.destination_fingerprint.clone(),
            path_fingerprint(&target),
        );
        owner.replacing_anchor = true;
        owner.pending_anchor = Some(create_pending_reference(&target, &paths).unwrap());
        publish_owner(&paths.owner, &owner).unwrap();
        finish_pending_anchor(&paths, &mut owner).unwrap();
        let pending = create_pending_reference(&paths.anchor, &paths).unwrap();
        owner.pending_reference = Some(pending.clone());
        write_owner(&paths.owner, &owner).unwrap();
        publish_pending_reference(&paths, &pending, &paths.destination).unwrap();

        let recovery = reconciliation_reference_plan(&source, &destination, &state);
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        let recovered = managed_state(&enabled, &paths).unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert!(recovered.owner.unwrap().pending_reference.is_none());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_missing_bound_reference_is_recreated_without_invalid_owner_state() {
        let (_temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        delete_reference_for_test(&destination.join("demo"));

        let recovery = reconciliation_reference_plan(&source, &destination, &state);
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        assert!(destination.join("demo/SKILL.md").is_file());
        assert!(managed_state(&enabled, &prepare_paths(&enabled).unwrap()).is_ok());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn rollback_refuses_a_missing_previous_source_without_hiding_current_source() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let replacement = temporary.path().join("replacement-source");
        fs::create_dir_all(replacement.join("demo")).unwrap();
        fs::write(replacement.join("demo/SKILL.md"), "# Replacement\n").unwrap();
        let receipt =
            apply_skill_reference(&plan(&replacement, &destination, &state, true)).unwrap();
        fs::rename(source.join("demo"), temporary.path().join("missing-demo")).unwrap();

        assert!(receipt.rollback().is_err());
        assert_eq!(
            fs::read_to_string(destination.join("demo/SKILL.md")).unwrap(),
            "# Replacement\n"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_anchor_retirement_checkpoint_converges_after_interruption() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        let relocated = temporary.path().join("relocated");
        fs::create_dir_all(relocated.join("demo")).unwrap();
        fs::write(relocated.join("demo/SKILL.md"), "# Relocated\n").unwrap();
        let relocated_plan = plan(&relocated, &destination, &state, true);
        let relocated_source = fs::canonicalize(relocated.join("demo")).unwrap();
        let relocated_fingerprint = path_fingerprint(&relocated_source);
        let mut owner = managed_state(&enabled, &paths)
            .unwrap()
            .owner
            .unwrap()
            .transitioning_to(&relocated_fingerprint);
        owner.replacing_anchor = true;
        write_owner(&paths.owner, &owner).unwrap();

        apply_skill_reference(&relocated_plan)
            .unwrap()
            .commit()
            .unwrap();

        let recovered = managed_state(&relocated_plan, &paths).unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert_eq!(
            recovered.anchor_source.as_deref(),
            Some(relocated_fingerprint.as_str())
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_interrupted_anchor_replacement_can_reconcile_to_disabled() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        let relocated = temporary.path().join("relocated");
        fs::create_dir_all(relocated.join("demo")).unwrap();
        fs::write(relocated.join("demo/SKILL.md"), "# Relocated\n").unwrap();
        let relocated_source = fs::canonicalize(relocated.join("demo")).unwrap();
        let relocated_fingerprint = path_fingerprint(&relocated_source);
        let mut owner = managed_state(&enabled, &paths)
            .unwrap()
            .owner
            .unwrap()
            .transitioning_to(&relocated_fingerprint);
        owner.replacing_anchor = true;
        write_owner(&paths.owner, &owner).unwrap();
        finish_anchor_retirement(&paths, &mut owner).unwrap();

        let recovery = reconciliation_reference_plan_for(&relocated, &destination, &state, false);
        assert!(!recovery.enabled());
        apply_skill_reference(&recovery).unwrap().commit().unwrap();

        let recovered = managed_state(&recovery, &paths).unwrap();
        let owner = recovered.owner.unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert!(owner.pending_source.is_none());
        assert!(!owner.replacing_anchor);
        assert!(!destination.join("demo/SKILL.md").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn direct_unified_discovery_still_reconciles_private_reference_state() {
        let (temporary, source, destination, state) = roots();
        let enabled = SkillReferencePlan::new(
            "owner/repo:demo",
            AppType::Codex,
            &source,
            &destination,
            &state,
            "demo",
            true,
        );
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        let unified = temporary.path().join("unified");
        fs::create_dir_all(unified.join("demo")).unwrap();
        fs::write(unified.join("demo/SKILL.md"), "# Unified\n").unwrap();

        let runtime = crate::SkillRuntime::try_new(
            &unified,
            &unified,
            [
                crate::SkillAppRuntime::try_new(AppType::Codex, &destination, &state, None)
                    .unwrap(),
            ],
        )
        .unwrap();
        let catalog = [catalog_entry_for("enabled_codex", true)];
        let snapshot = crate::inspect_installed_skills(&catalog, &runtime).unwrap();
        assert_eq!(
            snapshot[0].apps().next().unwrap().reason(),
            Some(crate::SkillControlReason::ManagedReferenceDrift)
        );
        assert!(matches!(
            crate::prepare_skill_switch(
                &catalog,
                "owner/repo:demo",
                &runtime,
                &AppType::Codex,
                true,
            ),
            Err(crate::SkillPrepareError::Unavailable {
                reason: Some(crate::SkillControlReason::ManagedReferenceDrift),
                ..
            })
        ));
        let live = crate::prepare_skill_reconciliation(
            &catalog,
            "owner/repo:demo",
            &runtime,
            &AppType::Codex,
        )
        .unwrap();
        let reference = live.reference().unwrap();
        assert!(live.target_enabled());
        assert!(!reference.enabled());
        apply_skill_reference(reference).unwrap().commit().unwrap();

        let recovered = managed_state(reference, &paths).unwrap();
        let owner = recovered.owner.unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert!(owner.pending_source.is_none());
        assert!(!owner.replacing_anchor);
        assert!(!destination.join("demo/SKILL.md").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_replaced_retiring_anchor_is_never_adopted() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let paths = prepare_paths(&enabled).unwrap();
        let relocated = temporary.path().join("relocated");
        fs::create_dir_all(relocated.join("demo")).unwrap();
        fs::write(relocated.join("demo/SKILL.md"), "# Relocated\n").unwrap();
        let relocated_plan = plan(&relocated, &destination, &state, true);
        let relocated_source = fs::canonicalize(relocated.join("demo")).unwrap();
        let mut owner = managed_state(&enabled, &paths)
            .unwrap()
            .owner
            .unwrap()
            .transitioning_to(&path_fingerprint(&relocated_source));
        owner.replacing_anchor = true;
        write_owner(&paths.owner, &owner).unwrap();
        let declared_target = reference_target(&paths.anchor).unwrap();
        delete_reference_for_test(&paths.anchor);
        create_reference(&declared_target, &paths.anchor).unwrap();

        assert!(matches!(
            apply_skill_reference(&relocated_plan),
            Err(SkillReferenceError::Unowned { .. })
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_retired_anchor_checkpoint_converges_after_interruption() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let public_identity = match inspect_reference(&destination.join("demo")).unwrap() {
            ReferenceEntry::Reference { identity, .. } => identity,
            _ => panic!("managed destination must be a reference"),
        };
        let paths = prepare_paths(&enabled).unwrap();
        let previous = managed_state(&enabled, &paths).unwrap();

        let relocated = temporary.path().join("relocated");
        fs::rename(&source, &relocated).unwrap();
        let relocated_plan = plan(&relocated, &destination, &state, true);
        let relocated_source = fs::canonicalize(relocated.join("demo")).unwrap();
        let relocated_fingerprint = path_fingerprint(&relocated_source);
        let mut owner = previous
            .owner
            .unwrap()
            .transitioning_to(&relocated_fingerprint);
        let previous_anchor = owner.anchor_identity.unwrap();
        owner.replacing_anchor = true;
        write_owner(&paths.owner, &owner).unwrap();
        let retired = paths.state_retired_anchor(previous_anchor);
        move_noreplace(&paths.anchor, &retired).unwrap();
        sync_move_parents(&paths.anchor, &retired).unwrap();

        apply_skill_reference(&relocated_plan)
            .unwrap()
            .commit()
            .unwrap();

        assert!(destination.join("demo/SKILL.md").is_file());
        assert!(matches!(
            inspect_reference(&destination.join("demo")).unwrap(),
            ReferenceEntry::Reference { identity, .. } if identity == public_identity
        ));
        let recovered = managed_state(&relocated_plan, &paths).unwrap();
        assert_eq!(recovered.location, ManagedLocation::Destination);
        assert_eq!(
            recovered.anchor_source.as_deref(),
            Some(relocated_fingerprint.as_str())
        );
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

    #[cfg(any(unix, windows))]
    #[test]
    fn observation_requires_the_current_source() {
        let (temporary, source, destination, state) = roots();
        let enabled = plan(&source, &destination, &state, true);
        apply_skill_reference(&enabled).unwrap().commit().unwrap();
        let other = temporary.path().join("other/demo");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("SKILL.md"), "# Other\n").unwrap();

        assert_eq!(
            inspect_skill_reference(
                &state,
                &AppType::Claude,
                "owner/repo:demo",
                "demo",
                other.parent().unwrap(),
                Some(&other),
                &destination,
            ),
            SkillReferenceObservation::ManagedStale
        );
        assert!(destination.join("demo/SKILL.md").is_file());
    }
}
