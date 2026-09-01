use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    read::{resolve_root, roots_overlap},
    tree::{copy_tree, digest_tree, ScanBudget, TreeError, MAX_TREE_DEPTH},
};

pub(super) const MANAGED_MARKER: &str = ".cc-switch-managed.json";
const MARKER_VERSION: u8 = 1;
const MAX_MARKER_BYTES: u64 = 4096;
const MAX_OPERATION_ENTRIES: usize = 50_000;
const MAX_OPERATION_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How Core materializes one Skill in an application's native directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSyncMethod {
    /// Prefer a symbolic link and fall back to a verified copy.
    #[default]
    Auto,
    /// Require a symbolic link.
    Symlink,
    /// Create a Core-marked, verified copy.
    Copy,
}

/// One idempotent native-directory intent prepared by Core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDeploymentPlan {
    skill_id: String,
    source_root: PathBuf,
    destination_root: PathBuf,
    directory: String,
    enabled: bool,
    sync_method: SkillSyncMethod,
    allow_matching_copy: bool,
}

impl SkillDeploymentPlan {
    pub(super) fn new(
        skill_id: impl Into<String>,
        source_root: impl Into<PathBuf>,
        destination_root: impl Into<PathBuf>,
        directory: impl Into<String>,
        enabled: bool,
        sync_method: SkillSyncMethod,
        allow_matching_copy: bool,
    ) -> Self {
        Self {
            skill_id: skill_id.into(),
            source_root: source_root.into(),
            destination_root: destination_root.into(),
            directory: directory.into(),
            enabled,
            sync_method,
            allow_matching_copy,
        }
    }

    pub fn skill_id(&self) -> &str {
        &self.skill_id
    }

    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub fn destination_root(&self) -> &Path {
        &self.destination_root
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn sync_method(&self) -> SkillSyncMethod {
        self.sync_method
    }
}

struct DeploymentPaths {
    source_root: PathBuf,
    destination_root: PathBuf,
    source: PathBuf,
    destination: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
}

#[derive(Debug)]
enum DeploymentExpectation {
    Missing,
    Linked {
        target: PathBuf,
    },
    Copy {
        digest: String,
        skill_id: String,
        directory: String,
    },
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
    RemovedLink {
        destination: PathBuf,
        target: PathBuf,
    },
    RemovedCopy {
        destination: PathBuf,
        backup: PathBuf,
        expectation: DeploymentExpectation,
    },
}

/// A native-directory change that the host must commit or roll back.
#[derive(Debug)]
#[must_use = "a Skill deployment receipt must be committed or rolled back"]
pub struct SkillDeploymentReceipt {
    change: DeploymentChange,
}

impl SkillDeploymentReceipt {
    /// Verifies that the visible destination still matches the applied change.
    pub fn verify(&self) -> Result<(), SkillDeploymentError> {
        match &self.change {
            DeploymentChange::Observed {
                destination,
                expectation,
            }
            | DeploymentChange::Created {
                destination,
                expectation,
            } => require_expectation(destination, expectation),
            DeploymentChange::RemovedLink { destination, .. }
            | DeploymentChange::RemovedCopy { destination, .. } => {
                require_expectation(destination, &DeploymentExpectation::Missing)
            }
        }
    }

    /// Finalizes hidden cleanup after the host commits its catalog transaction.
    pub fn commit(self) -> Result<(), SkillDeploymentError> {
        self.verify()?;
        if let DeploymentChange::RemovedCopy {
            backup,
            expectation,
            ..
        } = self.change
        {
            require_expectation(&backup, &expectation)?;
            remove_owned_directory(&backup)?;
            sync_parent(&backup)?;
        }
        Ok(())
    }

    /// Restores the native state observed before this operation.
    pub fn rollback(self) -> Result<(), SkillDeploymentError> {
        match self.change {
            DeploymentChange::Observed { .. } => Ok(()),
            DeploymentChange::Created {
                destination,
                expectation,
            } => {
                require_expectation(&destination, &expectation)?;
                remove_expected_deployment(&destination, &expectation)
            }
            DeploymentChange::RemovedLink {
                destination,
                target,
            } => {
                require_expectation(&destination, &DeploymentExpectation::Missing)?;
                create_symlink(&target, &destination)?;
                sync_parent(&destination)
            }
            DeploymentChange::RemovedCopy {
                destination,
                backup,
                expectation,
            } => {
                require_expectation(&destination, &DeploymentExpectation::Missing)?;
                require_expectation(&backup, &expectation)?;
                rename_no_replace(&backup, &destination)
            }
        }
    }
}

/// Applies one prepared directory intent. Repeating the same plan converges.
///
/// The caller must hold the shared live-config lock for the full receipt
/// lifecycle. Programs that ignore that advisory lock can still race normal
/// filesystem APIs; Core therefore uses no-replace renames and ownership
/// markers before removing directories. An I/O error may be returned after a
/// rename became visible; callers must reconcile from their committed
/// selection instead of inferring that no change happened.
pub fn apply_skill_deployment(
    plan: &SkillDeploymentPlan,
) -> Result<SkillDeploymentReceipt, SkillDeploymentError> {
    let paths = deployment_paths(plan)?;
    validate_source(&paths.source)?;
    validate_root_pair(&paths.source_root, &paths.destination_root)?;
    create_destination_root(&paths.destination_root)?;
    validate_root_pair(&paths.source_root, &paths.destination_root)?;
    recover_artifacts(plan, &paths)?;

    let mut budget = operation_budget();
    let state = inspect_deployment(plan, &paths, &mut budget)?;
    if plan.enabled {
        match state {
            DeploymentState::Missing => enable(plan, paths),
            DeploymentState::Linked { target } => Ok(observed_receipt(
                paths.destination,
                DeploymentExpectation::Linked { target },
            )),
            DeploymentState::Copy { digest } => Ok(observed_receipt(
                paths.destination,
                copy_expectation(plan, digest),
            )),
        }
    } else {
        match state {
            DeploymentState::Missing => Ok(observed_receipt(
                paths.destination,
                DeploymentExpectation::Missing,
            )),
            DeploymentState::Linked { target } => {
                require_expectation(
                    &paths.destination,
                    &DeploymentExpectation::Linked {
                        target: target.clone(),
                    },
                )?;
                remove_link(&paths.destination)?;
                sync_parent(&paths.destination)?;
                Ok(SkillDeploymentReceipt {
                    change: DeploymentChange::RemovedLink {
                        destination: paths.destination,
                        target,
                    },
                })
            }
            DeploymentState::Copy { digest } => {
                let claimed = ensure_managed_marker(plan, &paths.destination, &digest)?;
                let expectation = copy_expectation(plan, digest);
                if let Err(error) = require_expectation(&paths.destination, &expectation) {
                    if claimed {
                        remove_claimed_marker(plan, &paths.destination, &expectation)?;
                    }
                    return Err(error);
                }
                if let Err(error) = rename_no_replace(&paths.destination, &paths.backup) {
                    if claimed && !is_missing(&paths.destination)? {
                        remove_claimed_marker(plan, &paths.destination, &expectation)?;
                    }
                    return Err(error);
                }
                Ok(SkillDeploymentReceipt {
                    change: DeploymentChange::RemovedCopy {
                        destination: paths.destination,
                        backup: paths.backup,
                        expectation,
                    },
                })
            }
        }
    }
}

fn enable(
    plan: &SkillDeploymentPlan,
    paths: DeploymentPaths,
) -> Result<SkillDeploymentReceipt, SkillDeploymentError> {
    if plan.sync_method != SkillSyncMethod::Copy {
        match create_symlink(&paths.source, &paths.destination) {
            Ok(()) => {
                sync_parent(&paths.destination)?;
                return Ok(SkillDeploymentReceipt {
                    change: DeploymentChange::Created {
                        destination: paths.destination,
                        expectation: DeploymentExpectation::Linked {
                            target: paths.source,
                        },
                    },
                });
            }
            Err(error) if plan.sync_method == SkillSyncMethod::Symlink => return Err(error),
            Err(error) => {
                if !is_missing(&paths.destination)? {
                    return Err(error);
                }
            }
        }
    }
    create_copy(plan, paths)
}

fn create_copy(
    plan: &SkillDeploymentPlan,
    paths: DeploymentPaths,
) -> Result<SkillDeploymentReceipt, SkillDeploymentError> {
    create_private_directory(&paths.stage)?;
    write_marker(&paths.stage, plan, MarkerState::Staging, None)?;
    crate::fs::sync_directory(&paths.stage)
        .map_err(|source| SkillDeploymentError::io(&paths.stage, source))?;
    let copied = (|| {
        let mut budget = operation_budget();
        let before = digest_tree(&paths.source, None, &mut budget)?;
        copy_tree(&paths.source, &paths.stage, &mut budget)?;
        let after = digest_tree(&paths.source, None, &mut budget)?;
        let staged = digest_tree(&paths.stage, Some(MANAGED_MARKER.as_ref()), &mut budget)?;
        if before != after || before != staged {
            return Err(SkillDeploymentError::SourceChanged {
                path: paths.source.clone(),
            });
        }
        let digest = encode_digest(before);
        write_marker(&paths.stage, plan, MarkerState::Managed, Some(&digest))?;
        crate::fs::sync_directory(&paths.stage)
            .map_err(|source| SkillDeploymentError::io(&paths.stage, source))?;
        Ok(digest)
    })();
    let digest = match copied {
        Ok(digest) => digest,
        Err(error) => return Err(cleanup_error(&paths.stage, error)),
    };
    if let Err(error) = rename_no_replace(&paths.stage, &paths.destination) {
        return Err(cleanup_error(&paths.stage, error));
    }
    Ok(SkillDeploymentReceipt {
        change: DeploymentChange::Created {
            destination: paths.destination,
            expectation: copy_expectation(plan, digest),
        },
    })
}

fn recover_artifacts(
    plan: &SkillDeploymentPlan,
    paths: &DeploymentPaths,
) -> Result<(), SkillDeploymentError> {
    recover_stage(plan, &paths.stage)?;
    let Some(marker) = read_marker_if_present(&paths.backup)? else {
        if is_missing(&paths.backup)? {
            return Ok(());
        }
        return Err(SkillDeploymentError::Conflict {
            path: paths.backup.clone(),
        });
    };
    validate_marker_identity(plan, &marker, MarkerState::Managed)?;

    if plan.enabled && is_missing(&paths.destination)? {
        let expectation = copy_expectation(
            plan,
            marker
                .digest
                .clone()
                .expect("validated managed markers contain a digest"),
        );
        match require_expectation(&paths.backup, &expectation) {
            Ok(()) => rename_no_replace(&paths.backup, &paths.destination)?,
            Err(SkillDeploymentError::Conflict { .. }) => {
                remove_owned_directory(&paths.backup)?;
                sync_parent(&paths.backup)?;
            }
            Err(error) => return Err(error),
        }
        return Ok(());
    }
    if plan.enabled {
        let mut budget = operation_budget();
        inspect_deployment(plan, paths, &mut budget)?;
    }
    remove_owned_directory(&paths.backup)?;
    sync_parent(&paths.backup)
}

fn recover_stage(plan: &SkillDeploymentPlan, stage: &Path) -> Result<(), SkillDeploymentError> {
    let metadata = match fs::symlink_metadata(stage) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SkillDeploymentError::io(stage, source)),
    };
    if !metadata.file_type().is_dir() {
        return Err(SkillDeploymentError::Conflict {
            path: stage.to_owned(),
        });
    }
    match read_marker_if_present(stage)? {
        Some(marker) => {
            if marker.version != MARKER_VERSION
                || marker.skill_id != plan.skill_id
                || marker.directory != plan.directory
            {
                return Err(SkillDeploymentError::Conflict {
                    path: stage.to_owned(),
                });
            }
            remove_owned_directory(stage)?;
        }
        None if directory_is_empty(stage)? => {
            fs::remove_dir(stage).map_err(|source| SkillDeploymentError::io(stage, source))?;
        }
        None => {
            return Err(SkillDeploymentError::Conflict {
                path: stage.to_owned(),
            })
        }
    }
    sync_parent(stage)
}

fn deployment_paths(plan: &SkillDeploymentPlan) -> Result<DeploymentPaths, SkillDeploymentError> {
    if !plan.source_root.is_absolute() {
        return Err(SkillDeploymentError::RelativeRoot {
            path: plan.source_root.clone(),
        });
    }
    if !plan.destination_root.is_absolute() {
        return Err(SkillDeploymentError::RelativeRoot {
            path: plan.destination_root.clone(),
        });
    }
    let artifact = artifact_key(&plan.skill_id, &plan.directory);
    Ok(DeploymentPaths {
        source_root: plan.source_root.clone(),
        destination_root: plan.destination_root.clone(),
        source: plan.source_root.join(&plan.directory),
        destination: plan.destination_root.join(&plan.directory),
        stage: plan
            .destination_root
            .join(format!(".cc-switch-skill-{artifact}.stage")),
        backup: plan
            .destination_root
            .join(format!(".cc-switch-skill-{artifact}.backup")),
    })
}

fn artifact_key(skill_id: &str, directory: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(skill_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(directory.as_bytes());
    encode_digest(hasher.finalize().into())[..24].to_owned()
}

fn validate_root_pair(source: &Path, destination: &Path) -> Result<(), SkillDeploymentError> {
    let source_resolved = resolve_root(source).map_err(|error| SkillDeploymentError::Root {
        message: error.to_string(),
    })?;
    let destination_resolved =
        resolve_root(destination).map_err(|error| SkillDeploymentError::Root {
            message: error.to_string(),
        })?;
    if roots_overlap(&source_resolved, &destination_resolved) {
        Err(SkillDeploymentError::OverlappingRoots {
            source_root: source.to_owned(),
            destination_root: destination.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn validate_source(source: &Path) -> Result<(), SkillDeploymentError> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(SkillDeploymentError::InvalidSource {
                path: source.to_owned(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillDeploymentError::MissingSource {
                path: source.to_owned(),
            })
        }
        Err(source_error) => return Err(SkillDeploymentError::io(source, source_error)),
    }
    let manifest = source.join("SKILL.md");
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SkillDeploymentError::InvalidSource { path: manifest }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillDeploymentError::InvalidSource { path: manifest })
        }
        Err(source) => return Err(SkillDeploymentError::io(&manifest, source)),
    }
    let marker = source.join(MANAGED_MARKER);
    match fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(SkillDeploymentError::InvalidSource { path: marker }),
        Err(source) => Err(SkillDeploymentError::io(&marker, source)),
    }
}

enum DeploymentState {
    Missing,
    Linked { target: PathBuf },
    Copy { digest: String },
}

pub(super) enum ManagedCopyObservation {
    Absent,
    Selected,
    Conflict,
    Unreadable,
}

pub(super) fn inspect_managed_copy(
    root: &Path,
    skill_id: &str,
    directory: &str,
    budget: &mut ScanBudget,
) -> ManagedCopyObservation {
    let marker = match read_marker_if_present(root) {
        Ok(Some(marker)) => marker,
        Ok(None) => return ManagedCopyObservation::Absent,
        Err(SkillDeploymentError::Io { .. }) => return ManagedCopyObservation::Unreadable,
        Err(_) => return ManagedCopyObservation::Conflict,
    };
    let Some(expected) = marker.digest.as_deref() else {
        return ManagedCopyObservation::Conflict;
    };
    if marker.version != MARKER_VERSION
        || marker.skill_id != skill_id
        || marker.directory != directory
        || marker.state != MarkerState::Managed
        || !valid_digest(expected)
    {
        return ManagedCopyObservation::Conflict;
    }
    match digest_tree(root, Some(MANAGED_MARKER.as_ref()), budget) {
        Ok(actual) if encode_digest(actual) == expected => ManagedCopyObservation::Selected,
        Ok(_) => ManagedCopyObservation::Conflict,
        Err(_) => ManagedCopyObservation::Unreadable,
    }
}

fn inspect_deployment(
    plan: &SkillDeploymentPlan,
    paths: &DeploymentPaths,
    budget: &mut ScanBudget,
) -> Result<DeploymentState, SkillDeploymentError> {
    let metadata = match fs::symlink_metadata(&paths.destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeploymentState::Missing)
        }
        Err(source) => return Err(SkillDeploymentError::io(&paths.destination, source)),
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&paths.destination)
            .map_err(|source| SkillDeploymentError::io(&paths.destination, source))?;
        let resolved = if target.is_absolute() {
            target.clone()
        } else {
            paths.destination_root.join(&target)
        };
        let expected = fs::canonicalize(&paths.source)
            .map_err(|source| SkillDeploymentError::io(&paths.source, source))?;
        let actual = fs::canonicalize(&resolved)
            .map_err(|source| SkillDeploymentError::io(&paths.destination, source))?;
        return if actual == expected {
            Ok(DeploymentState::Linked { target })
        } else {
            Err(SkillDeploymentError::Conflict {
                path: paths.destination.clone(),
            })
        };
    }
    if !metadata.file_type().is_dir() {
        return Err(SkillDeploymentError::Conflict {
            path: paths.destination.clone(),
        });
    }
    if let Some(marker) = read_marker_if_present(&paths.destination)? {
        validate_marker_identity(plan, &marker, MarkerState::Managed)?;
        let digest = marker
            .digest
            .ok_or_else(|| SkillDeploymentError::Conflict {
                path: paths.destination.clone(),
            })?;
        let actual = encode_digest(digest_tree(
            &paths.destination,
            Some(MANAGED_MARKER.as_ref()),
            budget,
        )?);
        return if actual == digest {
            Ok(DeploymentState::Copy { digest })
        } else {
            Err(SkillDeploymentError::Conflict {
                path: paths.destination.clone(),
            })
        };
    }
    if plan.allow_matching_copy {
        let source = digest_tree(&paths.source, None, budget)?;
        let destination = digest_tree(&paths.destination, None, budget)?;
        if source == destination {
            return Ok(DeploymentState::Copy {
                digest: encode_digest(destination),
            });
        }
    }
    Err(SkillDeploymentError::Conflict {
        path: paths.destination.clone(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum MarkerState {
    Staging,
    Managed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedMarker {
    version: u8,
    skill_id: String,
    directory: String,
    state: MarkerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
}

fn ensure_managed_marker(
    plan: &SkillDeploymentPlan,
    destination: &Path,
    digest: &str,
) -> Result<bool, SkillDeploymentError> {
    if read_marker_if_present(destination)?.is_none() {
        write_marker(destination, plan, MarkerState::Managed, Some(digest))?;
        crate::fs::sync_directory(destination)
            .map_err(|source| SkillDeploymentError::io(destination, source))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn remove_claimed_marker(
    plan: &SkillDeploymentPlan,
    destination: &Path,
    expectation: &DeploymentExpectation,
) -> Result<(), SkillDeploymentError> {
    let DeploymentExpectation::Copy {
        digest,
        skill_id,
        directory,
    } = expectation
    else {
        unreachable!("only copied legacy deployments are claimed")
    };
    if skill_id != &plan.skill_id || directory != &plan.directory {
        return Err(SkillDeploymentError::Conflict {
            path: destination.to_owned(),
        });
    }
    let marker =
        read_marker_if_present(destination)?.ok_or_else(|| SkillDeploymentError::Conflict {
            path: destination.to_owned(),
        })?;
    if marker.version != MARKER_VERSION
        || marker.state != MarkerState::Managed
        || marker.skill_id != plan.skill_id
        || marker.directory != plan.directory
        || marker.digest.as_deref() != Some(digest)
    {
        return Err(SkillDeploymentError::Conflict {
            path: destination.to_owned(),
        });
    }
    let path = destination.join(MANAGED_MARKER);
    fs::remove_file(&path).map_err(|source| SkillDeploymentError::io(&path, source))?;
    crate::fs::sync_directory(destination)
        .map_err(|source| SkillDeploymentError::io(destination, source))
}

fn write_marker(
    root: &Path,
    plan: &SkillDeploymentPlan,
    state: MarkerState,
    digest: Option<&str>,
) -> Result<(), SkillDeploymentError> {
    let marker = ManagedMarker {
        version: MARKER_VERSION,
        skill_id: plan.skill_id.clone(),
        directory: plan.directory.clone(),
        state,
        digest: digest.map(str::to_owned),
    };
    let contents = serde_json::to_vec(&marker).map_err(|error| SkillDeploymentError::Marker {
        path: root.join(MANAGED_MARKER),
        message: error.to_string(),
    })?;
    if contents.len() as u64 > MAX_MARKER_BYTES {
        return Err(SkillDeploymentError::Marker {
            path: root.join(MANAGED_MARKER),
            message: "marker is too large".to_owned(),
        });
    }
    crate::fs::atomic_write_private(&root.join(MANAGED_MARKER), &contents).map_err(|error| {
        SkillDeploymentError::Marker {
            path: root.join(MANAGED_MARKER),
            message: error.to_string(),
        }
    })
}

fn read_marker_if_present(root: &Path) -> Result<Option<ManagedMarker>, SkillDeploymentError> {
    let path = root.join(MANAGED_MARKER);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => {
            return Err(SkillDeploymentError::Marker {
                path,
                message: "marker is not a regular file".to_owned(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SkillDeploymentError::io(path, source)),
    };
    if metadata.len() > MAX_MARKER_BYTES {
        return Err(SkillDeploymentError::Marker {
            path,
            message: "marker is too large".to_owned(),
        });
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    File::open(&path)
        .and_then(|file| file.take(MAX_MARKER_BYTES + 1).read_to_end(&mut contents))
        .map_err(|source| SkillDeploymentError::io(&path, source))?;
    if contents.len() as u64 > MAX_MARKER_BYTES {
        return Err(SkillDeploymentError::Marker {
            path,
            message: "marker is too large".to_owned(),
        });
    }
    serde_json::from_slice(&contents)
        .map(Some)
        .map_err(|error| SkillDeploymentError::Marker {
            path,
            message: error.to_string(),
        })
}

fn validate_marker_identity(
    plan: &SkillDeploymentPlan,
    marker: &ManagedMarker,
    state: MarkerState,
) -> Result<(), SkillDeploymentError> {
    if marker.version == MARKER_VERSION
        && marker.skill_id == plan.skill_id
        && marker.directory == plan.directory
        && marker.state == state
        && (state == MarkerState::Staging || marker.digest.as_deref().is_some_and(valid_digest))
    {
        Ok(())
    } else {
        Err(SkillDeploymentError::Marker {
            path: plan.destination_root.join(&plan.directory),
            message: "marker identity is invalid".to_owned(),
        })
    }
}

fn require_expectation(
    path: &Path,
    expectation: &DeploymentExpectation,
) -> Result<(), SkillDeploymentError> {
    match expectation {
        DeploymentExpectation::Missing => {
            if is_missing(path)? {
                Ok(())
            } else {
                Err(SkillDeploymentError::Conflict {
                    path: path.to_owned(),
                })
            }
        }
        DeploymentExpectation::Linked { target } => {
            let metadata = fs::symlink_metadata(path)
                .map_err(|source| SkillDeploymentError::io(path, source))?;
            if metadata.file_type().is_symlink()
                && fs::read_link(path).map_err(|source| SkillDeploymentError::io(path, source))?
                    == *target
            {
                Ok(())
            } else {
                Err(SkillDeploymentError::Conflict {
                    path: path.to_owned(),
                })
            }
        }
        DeploymentExpectation::Copy {
            digest,
            skill_id,
            directory,
        } => {
            let marker =
                read_marker_if_present(path)?.ok_or_else(|| SkillDeploymentError::Conflict {
                    path: path.to_owned(),
                })?;
            if marker.version != MARKER_VERSION
                || marker.state != MarkerState::Managed
                || marker.skill_id != *skill_id
                || marker.directory != *directory
                || marker.digest.as_deref() != Some(digest)
            {
                return Err(SkillDeploymentError::Conflict {
                    path: path.to_owned(),
                });
            }
            let mut budget = operation_budget();
            let actual = encode_digest(digest_tree(
                path,
                Some(MANAGED_MARKER.as_ref()),
                &mut budget,
            )?);
            if actual == *digest {
                Ok(())
            } else {
                Err(SkillDeploymentError::Conflict {
                    path: path.to_owned(),
                })
            }
        }
    }
}

fn remove_expected_deployment(
    path: &Path,
    expectation: &DeploymentExpectation,
) -> Result<(), SkillDeploymentError> {
    match expectation {
        DeploymentExpectation::Linked { .. } => remove_link(path),
        DeploymentExpectation::Copy { .. } => remove_owned_directory(path),
        DeploymentExpectation::Missing => Ok(()),
    }?;
    sync_parent(path)
}

fn remove_owned_directory(root: &Path) -> Result<(), SkillDeploymentError> {
    let metadata =
        fs::symlink_metadata(root).map_err(|source| SkillDeploymentError::io(root, source))?;
    if !metadata.file_type().is_dir() {
        return Err(SkillDeploymentError::Conflict {
            path: root.to_owned(),
        });
    }
    let mut budget = operation_budget();
    remove_owned_entries(root, root, 0, &mut budget)?;
    fs::remove_file(root.join(MANAGED_MARKER))
        .map_err(|source| SkillDeploymentError::io(root.join(MANAGED_MARKER), source))?;
    fs::remove_dir(root).map_err(|source| SkillDeploymentError::io(root, source))
}

fn remove_owned_entries(
    root: &Path,
    directory: &Path,
    depth: usize,
    budget: &mut ScanBudget,
) -> Result<(), SkillDeploymentError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::DepthLimit {
            limit: MAX_TREE_DEPTH,
        }
        .into());
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|source| SkillDeploymentError::io(directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SkillDeploymentError::io(directory, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if directory == root && entry.file_name() == MANAGED_MARKER {
            continue;
        }
        budget.charge_entries(1)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| SkillDeploymentError::io(&path, source))?;
        if metadata.file_type().is_dir() {
            remove_owned_entries(root, &path, depth.saturating_add(1), budget)?;
            fs::remove_dir(&path).map_err(|source| SkillDeploymentError::io(&path, source))?;
        } else {
            fs::remove_file(&path).map_err(|source| SkillDeploymentError::io(&path, source))?;
        }
    }
    Ok(())
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

fn copy_expectation(plan: &SkillDeploymentPlan, digest: String) -> DeploymentExpectation {
    DeploymentExpectation::Copy {
        digest,
        skill_id: plan.skill_id.clone(),
        directory: plan.directory.clone(),
    }
}

fn create_destination_root(path: &Path) -> Result<(), SkillDeploymentError> {
    fs::create_dir_all(path).map_err(|source| SkillDeploymentError::io(path, source))
}

fn create_private_directory(path: &Path) -> Result<(), SkillDeploymentError> {
    crate::fs::create_private_directory(path)
        .map_err(|source| SkillDeploymentError::io(path, source))
}

fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), SkillDeploymentError> {
    crate::fs::move_path_no_replace(source, destination).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            SkillDeploymentError::Conflict {
                path: destination.to_owned(),
            }
        } else {
            SkillDeploymentError::io(destination, source)
        }
    })?;
    sync_parent(destination)
}

fn sync_parent(path: &Path) -> Result<(), SkillDeploymentError> {
    let parent = path
        .parent()
        .ok_or_else(|| SkillDeploymentError::Conflict {
            path: path.to_owned(),
        })?;
    crate::fs::sync_directory(parent).map_err(|source| SkillDeploymentError::Durability {
        path: parent.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn create_symlink(source: &Path, destination: &Path) -> Result<(), SkillDeploymentError> {
    std::os::unix::fs::symlink(source, destination)
        .map_err(|error| SkillDeploymentError::io(destination, error))
}

#[cfg(windows)]
fn create_symlink(source: &Path, destination: &Path) -> Result<(), SkillDeploymentError> {
    std::os::windows::fs::symlink_dir(source, destination)
        .map_err(|error| SkillDeploymentError::io(destination, error))
}

#[cfg(unix)]
fn remove_link(path: &Path) -> Result<(), SkillDeploymentError> {
    fs::remove_file(path).map_err(|source| SkillDeploymentError::io(path, source))
}

#[cfg(windows)]
fn remove_link(path: &Path) -> Result<(), SkillDeploymentError> {
    fs::remove_dir(path).map_err(|source| SkillDeploymentError::io(path, source))
}

fn is_missing(path: &Path) -> Result<bool, SkillDeploymentError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(SkillDeploymentError::io(path, source)),
    }
}

fn directory_is_empty(path: &Path) -> Result<bool, SkillDeploymentError> {
    fs::read_dir(path)
        .map_err(|source| SkillDeploymentError::io(path, source))?
        .next()
        .transpose()
        .map(|entry| entry.is_none())
        .map_err(|source| SkillDeploymentError::io(path, source))
}

fn cleanup_error(path: &Path, error: SkillDeploymentError) -> SkillDeploymentError {
    match read_marker_if_present(path).and_then(|marker| {
        if marker.is_some() {
            remove_owned_directory(path)?;
            sync_parent(path)
        } else if directory_is_empty(path)? {
            fs::remove_dir(path).map_err(|source| SkillDeploymentError::io(path, source))
        } else {
            Err(SkillDeploymentError::Conflict {
                path: path.to_owned(),
            })
        }
    }) {
        Ok(()) => error,
        Err(cleanup) => SkillDeploymentError::Recovery {
            message: format!("{error}; cleanup failed: {cleanup}"),
        },
    }
}

fn operation_budget() -> ScanBudget {
    ScanBudget::new(MAX_OPERATION_ENTRIES, MAX_OPERATION_BYTES)
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

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl From<TreeError> for SkillDeploymentError {
    fn from(error: TreeError) -> Self {
        Self::Tree {
            message: error.to_string(),
        }
    }
}

/// A safe deployment could not be inspected or applied.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillDeploymentError {
    #[error("Skill root must be absolute: {path:?}")]
    RelativeRoot { path: PathBuf },
    #[error("Skill source does not exist: {path:?}")]
    MissingSource { path: PathBuf },
    #[error("Skill source is invalid: {path:?}")]
    InvalidSource { path: PathBuf },
    #[error("Skill roots overlap: {source_root:?}, {destination_root:?}")]
    OverlappingRoots {
        source_root: PathBuf,
        destination_root: PathBuf,
    },
    #[error("Skill root validation failed: {message}")]
    Root { message: String },
    #[error("native Skill destination conflicts with managed state: {path:?}")]
    Conflict { path: PathBuf },
    #[error("Skill source changed while it was copied: {path:?}")]
    SourceChanged { path: PathBuf },
    #[error("invalid Skill ownership marker at {path:?}: {message}")]
    Marker { path: PathBuf, message: String },
    #[error("invalid Skill tree: {message}")]
    Tree { message: String },
    #[error("Skill filesystem I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("filesystem change at {path:?} is visible but not durable: {source}")]
    Durability {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Skill recovery is incomplete: {message}")]
    Recovery { message: String },
}

impl SkillDeploymentError {
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

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(source.join("demo")).unwrap();
        fs::write(source.join("demo/SKILL.md"), "# Demo\n").unwrap();
        (temporary, source, destination)
    }

    fn plan(source: &Path, destination: &Path, enabled: bool) -> SkillDeploymentPlan {
        SkillDeploymentPlan::new(
            "owner/repo:demo",
            source,
            destination,
            "demo",
            enabled,
            SkillSyncMethod::Copy,
            false,
        )
    }

    #[test]
    fn copied_deployments_are_idempotent_and_reversible() {
        let (_temporary, source, destination) = roots();
        let enabled = plan(&source, &destination, true);
        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        let disabled = plan(&source, &destination, false);
        let receipt = apply_skill_deployment(&disabled).unwrap();
        assert!(!destination.join("demo").exists());
        receipt.rollback().unwrap();
        assert!(destination.join("demo/SKILL.md").is_file());

        apply_skill_deployment(&disabled).unwrap().commit().unwrap();
        assert!(!destination.join("demo").exists());
    }

    #[test]
    fn external_destinations_are_never_removed() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(destination.join("demo")).unwrap();
        fs::write(destination.join("demo/SKILL.md"), "external").unwrap();

        let error = apply_skill_deployment(&plan(&source, &destination, false)).unwrap_err();
        assert!(matches!(error, SkillDeploymentError::Conflict { .. }));
        assert_eq!(
            fs::read_to_string(destination.join("demo/SKILL.md")).unwrap(),
            "external"
        );
    }

    #[test]
    fn interrupted_copy_stages_are_cleaned_before_retry() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let enabled = plan(&source, &destination, true);
        let paths = deployment_paths(&enabled).unwrap();
        fs::create_dir(&paths.stage).unwrap();
        write_marker(&paths.stage, &enabled, MarkerState::Staging, None).unwrap();
        fs::write(paths.stage.join("partial"), "partial").unwrap();

        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        assert!(paths.destination.join("SKILL.md").is_file());
        assert!(!paths.stage.exists());
    }

    #[test]
    fn interrupted_disable_restores_a_committed_enabled_state() {
        let (_temporary, source, destination) = roots();
        let enabled = plan(&source, &destination, true);
        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        let disabled = plan(&source, &destination, false);
        let paths = deployment_paths(&disabled).unwrap();

        drop(apply_skill_deployment(&disabled).unwrap());
        assert!(!paths.destination.exists());
        assert!(paths.backup.exists());

        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        assert!(paths.destination.join("SKILL.md").is_file());
        assert!(!paths.backup.exists());
    }

    #[test]
    fn interrupted_changes_converge_to_a_committed_disabled_state() {
        let (_temporary, source, destination) = roots();
        let enabled = plan(&source, &destination, true);
        let paths = deployment_paths(&enabled).unwrap();

        drop(apply_skill_deployment(&enabled).unwrap());
        assert!(paths.destination.exists());
        apply_skill_deployment(&plan(&source, &destination, false))
            .unwrap()
            .commit()
            .unwrap();
        assert!(!paths.destination.exists());
        assert!(!paths.backup.exists());

        apply_skill_deployment(&enabled).unwrap().commit().unwrap();
        drop(apply_skill_deployment(&plan(&source, &destination, false)).unwrap());
        assert!(paths.backup.exists());
        apply_skill_deployment(&plan(&source, &destination, false))
            .unwrap()
            .commit()
            .unwrap();
        assert!(!paths.destination.exists());
        assert!(!paths.backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_deployments_do_not_touch_external_links() {
        let (temporary, source, destination) = roots();
        let external = temporary.path().join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("SKILL.md"), "external").unwrap();
        fs::create_dir(&destination).unwrap();
        std::os::unix::fs::symlink(&external, destination.join("demo")).unwrap();
        let mut deployment = plan(&source, &destination, false);
        deployment.sync_method = SkillSyncMethod::Symlink;

        assert!(matches!(
            apply_skill_deployment(&deployment),
            Err(SkillDeploymentError::Conflict { .. })
        ));
        assert_eq!(fs::read_link(destination.join("demo")).unwrap(), external);
    }
}
