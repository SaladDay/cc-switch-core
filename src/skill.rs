//! Shared contracts and guarded filesystem materialization for installed Skills.

use std::{
    fs::{self, DirEntry, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Table};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::operation::{LogicalTarget, MAX_OPERATION_CONTENT_BYTES};

/// Maximum number of entries accepted in one installed Skill tree.
pub const MAX_SKILL_TREE_ENTRIES: usize = 10_000;
/// Maximum total file content accepted in one installed Skill tree.
pub const MAX_SKILL_TREE_BYTES: u64 = 512 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEMP_DIRECTORY_PREFIX: &str = ".cc-switch-skill.";
const TEMP_MARKER_FILE: &str = ".operation.json";
const TEMP_MARKER_STAGING_FILE: &str = ".operation.json.pending";
const TEMP_MARKER_VERSION: u8 = 1;
const MAX_TEMP_MARKER_BYTES: u64 = 1024;
const MANAGED_COPY_MARKER_FILE: &str = ".cc-switch-managed.json";
const MANAGED_COPY_MARKER_VERSION: u8 = 1;
const MAX_MANAGED_COPY_MARKER_BYTES: u64 = 1024;

/// How an application finds installed Skills beyond its own native directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillDiscoveryMode {
    /// Core materializes the selected Skill in the application's native directory.
    Managed,
    /// The application also discovers `~/.agents/skills` directly.
    Unified,
}

/// Who owns the persisted per-application Skill selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillSelectionMode {
    /// The host product persists a requested selection for this application.
    HostManaged,
    /// Selection is controlled outside the host product.
    External,
}

/// Native document containing a supported per-Skill disabled list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillConfigTarget {
    GeminiSettings,
    GrokConfig,
    HermesConfig,
}

/// Effective state reported by a supported native per-Skill control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillConfigState {
    Enabled,
    Disabled,
    Required,
    GloballyDisabled,
    ExternallyDisabled,
}

impl SkillConfigTarget {
    /// Returns the shared logical document edited by this control.
    pub const fn logical_target(self) -> LogicalTarget {
        match self {
            Self::GeminiSettings => LogicalTarget::GeminiSettings,
            Self::GrokConfig => LogicalTarget::GrokConfig,
            Self::HermesConfig => LogicalTarget::HermesConfig,
        }
    }
}

/// Product-neutral Skill behavior declared by an application descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillAppContract {
    selection: SkillSelectionMode,
    discovery: SkillDiscoveryMode,
    config_target: Option<SkillConfigTarget>,
}

impl SkillAppContract {
    /// Declares an application whose requested selection is persisted by the host.
    pub const fn host_managed() -> Self {
        Self {
            selection: SkillSelectionMode::HostManaged,
            discovery: SkillDiscoveryMode::Managed,
            config_target: None,
        }
    }

    /// Declares an application whose selection is controlled externally.
    pub const fn externally_managed() -> Self {
        Self {
            selection: SkillSelectionMode::External,
            discovery: SkillDiscoveryMode::Managed,
            config_target: None,
        }
    }

    /// Declares that the application discovers `~/.agents/skills` directly.
    pub const fn with_unified_store_discovery(mut self) -> Self {
        self.discovery = SkillDiscoveryMode::Unified;
        self
    }

    /// Declares a native per-Skill configuration control.
    pub const fn with_config_target(mut self, target: SkillConfigTarget) -> Self {
        self.config_target = Some(target);
        self
    }

    /// Returns who owns the requested per-application selection.
    pub const fn selection(self) -> SkillSelectionMode {
        self.selection
    }

    /// Returns how this application discovers installed Skills.
    pub const fn discovery(self) -> SkillDiscoveryMode {
        self.discovery
    }

    /// Returns the native document used for per-Skill enablement, when supported.
    pub const fn config_target(self) -> Option<SkillConfigTarget> {
        self.config_target
    }
}

/// Why an application's effective Skill state cannot be represented as a normal switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillControlReason {
    ExternalDiscovery,
    DirectUnifiedDiscovery,
    Required,
    GloballyDisabled,
    ExternallyDisabled,
    NativeControlUnavailable,
}

/// Product-neutral routing decision for one Skill and application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillRoute {
    config_target: Option<SkillConfigTarget>,
    deploy_native: bool,
}

impl SkillRoute {
    pub const fn config_target(self) -> Option<SkillConfigTarget> {
        self.config_target
    }

    pub const fn deploy_native(self) -> bool {
        self.deploy_native
    }
}

/// Product-neutral effective state for one Skill and application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillEffectiveState {
    enabled: Option<bool>,
    reason: Option<SkillControlReason>,
}

impl SkillEffectiveState {
    pub const fn enabled(self) -> Option<bool> {
        self.enabled
    }

    pub const fn reason(self) -> Option<SkillControlReason> {
        self.reason
    }
}

/// Resolves whether a host may edit native configuration or deployment state.
pub fn resolve_skill_route(
    contract: SkillAppContract,
    discovery: SkillDiscoveryState,
) -> Result<SkillRoute, SkillControlReason> {
    if discovery == SkillDiscoveryState::External {
        return Err(SkillControlReason::ExternalDiscovery);
    }
    if contract.discovery() == SkillDiscoveryMode::Unified
        && contract.config_target().is_none()
        && discovery == SkillDiscoveryState::Selected
    {
        return Err(SkillControlReason::DirectUnifiedDiscovery);
    }
    Ok(SkillRoute {
        config_target: contract.config_target(),
        deploy_native: contract.discovery() == SkillDiscoveryMode::Managed
            || discovery == SkillDiscoveryState::Missing,
    })
}

/// Resolves the effective user-level state from observed native inputs.
pub fn resolve_skill_effective_state(
    contract: SkillAppContract,
    discovery: SkillDiscoveryState,
    native_enabled: Option<bool>,
    config_state: Option<SkillConfigState>,
) -> SkillEffectiveState {
    let constrained = |enabled, reason| SkillEffectiveState {
        enabled,
        reason: Some(reason),
    };
    if discovery == SkillDiscoveryState::External {
        return constrained(None, SkillControlReason::ExternalDiscovery);
    }
    if contract.discovery() == SkillDiscoveryMode::Unified
        && contract.config_target().is_none()
        && discovery == SkillDiscoveryState::Selected
    {
        return constrained(None, SkillControlReason::DirectUnifiedDiscovery);
    }
    if contract.config_target().is_none() {
        return SkillEffectiveState {
            enabled: native_enabled,
            reason: None,
        };
    }
    match config_state {
        Some(SkillConfigState::GloballyDisabled) => {
            constrained(Some(false), SkillControlReason::GloballyDisabled)
        }
        Some(SkillConfigState::ExternallyDisabled) => {
            constrained(None, SkillControlReason::ExternallyDisabled)
        }
        Some(SkillConfigState::Required) if native_enabled == Some(false) => SkillEffectiveState {
            enabled: Some(false),
            reason: None,
        },
        Some(SkillConfigState::Required) => {
            constrained(native_enabled, SkillControlReason::Required)
        }
        Some(state @ (SkillConfigState::Enabled | SkillConfigState::Disabled)) => {
            let configured = state == SkillConfigState::Enabled;
            SkillEffectiveState {
                enabled: if discovery == SkillDiscoveryState::Selected || !configured {
                    Some(configured)
                } else {
                    native_enabled
                },
                reason: None,
            }
        }
        None => constrained(None, SkillControlReason::NativeControlUnavailable),
    }
}

/// How an installed Skill is materialized in an application's native directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSyncMethod {
    /// Prefer a symbolic link and fall back to a Core-marked, verified copy.
    #[default]
    Auto,
    /// Require a symbolic link.
    Symlink,
    /// Materialize a Core-marked, verified copy.
    Copy,
}

/// Evidence a host may use when classifying an unmarked copied Skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillCopyPolicy {
    /// Only a Core ownership marker proves that a copied directory is managed.
    #[default]
    ManagedOnly,
    /// An unmarked directory may be treated as managed while it exactly matches the source.
    AllowMatching,
}

/// The verified state of one native Skill destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDeploymentState {
    Missing,
    Linked,
    Copied,
}

/// How a directly discovered Skill relates to the selected catalog source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiscoveryState {
    Missing,
    Selected,
    External,
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
    #[error("invalid Skill name: {name:?}")]
    InvalidName { name: String },
    #[error("invalid {target:?} Skill configuration: {message}")]
    InvalidConfig {
        target: SkillConfigTarget,
        message: String,
    },
    #[error("{target:?} cannot enable a Skill while Skills are disabled globally")]
    GloballyDisabled { target: SkillConfigTarget },
    #[error(
        "{target:?} cannot enable a Skill while a platform-specific native setting disables it"
    )]
    ExternallyDisabled { target: SkillConfigTarget },
    #[error("{target:?} requires Skill {name:?} to remain enabled")]
    RequiredSkill {
        target: SkillConfigTarget,
        name: String,
    },
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
    Copied { digest: String, directory: String },
    MatchingCopy { digest: String },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum InterruptedOperation {
    Enable,
    Disable,
}

#[derive(Debug, Serialize, Deserialize)]
struct InterruptedOperationMarker {
    version: u8,
    operation: InterruptedOperation,
    directory: String,
}

enum OperationMarkerFile {
    Missing,
    Invalid,
    Valid(InterruptedOperationMarker),
}

#[derive(Debug, Serialize, Deserialize)]
struct ManagedCopyMarker {
    version: u8,
    directory: String,
    source_digest: String,
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
        if let DeploymentChange::Removed {
            temporary_root,
            backup,
            ..
        } = self.change
        {
            remove_deployment(&backup)?;
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
                expectation: _,
            } => {
                require_expectation(&destination, &DeploymentExpectation::Missing)?;
                rename_path(&backup, &destination)?;
                remove_directory(&temporary_root)
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
        || directory.ends_with('.')
        || directory
            .chars()
            .any(|character| character.is_ascii_control() || "<>:\"|?*".contains(character))
        || is_windows_reserved_name(directory)
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

/// Returns a stable, conservative key for detecting cross-platform directory aliases.
pub fn skill_directory_key(directory: &str) -> Result<String, SkillConfigError> {
    validate_skill_directory(directory)?;
    Ok(directory.nfc().case_fold().nfc().collect())
}

fn is_windows_reserved_name(directory: &str) -> bool {
    let stem = directory.split('.').next().unwrap_or(directory);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

/// Resolves an absolute Skill path without requiring its final components to exist.
/// Hosts can compare these identities before allowing multiple applications to
/// share or nest native Skill roots.
pub fn skill_path_identity(path: &Path) -> Result<PathBuf, SkillConfigError> {
    if !path.is_absolute() {
        return Err(SkillConfigError::RelativeRoot {
            path: path.to_owned(),
        });
    }
    resolve_candidate(path)
}

/// Inspects a native destination without changing it.
pub fn inspect_skill_deployment(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
) -> Result<SkillDeploymentState, SkillConfigError> {
    inspect_skill_deployment_with_policy(
        source_root,
        destination_root,
        directory,
        SkillCopyPolicy::ManagedOnly,
    )
}

/// Inspects a native destination using host-supplied evidence for legacy copies.
pub fn inspect_skill_deployment_with_policy(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
    copy_policy: SkillCopyPolicy,
) -> Result<SkillDeploymentState, SkillConfigError> {
    let paths = deployment_paths(source_root, destination_root, directory)?;
    inspect_paths_with_policy(&paths.source, &paths.destination, directory, copy_policy)
        .map(|(state, _)| state)
}

/// Returns whether a native Skill directory is present, without claiming ownership of it.
pub fn inspect_skill_presence(
    destination_root: &Path,
    directory: &str,
) -> Result<bool, SkillConfigError> {
    validate_skill_directory(directory)?;
    if !destination_root.is_absolute() {
        return Err(SkillConfigError::RelativeRoot {
            path: destination_root.to_owned(),
        });
    }
    let destination = destination_root.join(directory);
    match fs::metadata(&destination) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(SkillConfigError::Conflict { path: destination }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(&destination) {
                Ok(_) => Err(SkillConfigError::Conflict { path: destination }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(source) => Err(SkillConfigError::io(destination, source)),
            }
        }
        Err(source) => Err(SkillConfigError::io(destination, source)),
    }
}

/// Identifies whether a directly discovered Skill is the selected source.
pub fn inspect_skill_discovery(
    source_root: &Path,
    discovery_root: &Path,
    directory: &str,
) -> Result<SkillDiscoveryState, SkillConfigError> {
    inspect_skill_discovery_with_policy(
        source_root,
        discovery_root,
        directory,
        SkillCopyPolicy::ManagedOnly,
    )
}

/// Inspects direct discovery using host-supplied evidence for legacy copies.
pub fn inspect_skill_discovery_with_policy(
    source_root: &Path,
    discovery_root: &Path,
    directory: &str,
    copy_policy: SkillCopyPolicy,
) -> Result<SkillDiscoveryState, SkillConfigError> {
    validate_skill_directory(directory)?;
    for root in [source_root, discovery_root] {
        if !root.is_absolute() {
            return Err(SkillConfigError::RelativeRoot {
                path: root.to_owned(),
            });
        }
    }
    validate_skill_source(&source_root.join(directory))?;
    if !inspect_skill_presence(discovery_root, directory)? {
        return Ok(SkillDiscoveryState::Missing);
    }
    if resolve_candidate(source_root)? == resolve_candidate(discovery_root)? {
        return Ok(SkillDiscoveryState::Selected);
    }
    match inspect_skill_deployment_with_policy(source_root, discovery_root, directory, copy_policy)
    {
        Ok(SkillDeploymentState::Missing) => Ok(SkillDiscoveryState::Missing),
        Ok(SkillDeploymentState::Linked | SkillDeploymentState::Copied) => {
            Ok(SkillDiscoveryState::Selected)
        }
        Err(SkillConfigError::Conflict { .. }) => Ok(SkillDiscoveryState::External),
        Err(error) => Err(error),
    }
}

/// Reads the effective state from a supported native per-Skill control.
pub fn inspect_skill_config_state(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
) -> Result<SkillConfigState, SkillConfigError> {
    validate_skill_name(name)?;
    if native_skill_is_required(target, name) {
        return Ok(SkillConfigState::Required);
    }
    let disabled = match target {
        SkillConfigTarget::GeminiSettings => {
            ensure_json_skill_control_writable(target, contents)?;
            let root = parse_skill_json(target, contents)?;
            if !json_skills_enabled(target, &root)? {
                return Ok(SkillConfigState::GloballyDisabled);
            }
            json_disabled_names(target, &root)?
        }
        SkillConfigTarget::GrokConfig => {
            toml_disabled_names(target, &parse_skill_toml(target, contents)?)?
        }
        SkillConfigTarget::HermesConfig => {
            let root = parse_skill_yaml(target, contents)?;
            if yaml_platform_disables_name(target, &root, name)? {
                return Ok(SkillConfigState::ExternallyDisabled);
            }
            yaml_disabled_names(target, &root)?
        }
    };
    Ok(
        if disabled
            .iter()
            .any(|entry| native_skill_names_equal(target, entry, name))
        {
            SkillConfigState::Disabled
        } else {
            SkillConfigState::Enabled
        },
    )
}

/// Changes only the supported native disabled list and preserves unrelated data.
/// `None` means the requested state already matches the document.
pub fn project_skill_config_enabled(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigError> {
    validate_skill_name(name)?;
    if !enabled && native_skill_is_required(target, name) {
        return Err(SkillConfigError::RequiredSkill {
            target,
            name: name.to_owned(),
        });
    }
    match target {
        SkillConfigTarget::GeminiSettings => {
            project_json_skill_enabled(target, contents, name, enabled)
        }
        SkillConfigTarget::GrokConfig => {
            project_toml_skill_enabled(target, contents, name, enabled)
        }
        SkillConfigTarget::HermesConfig => {
            project_yaml_skill_enabled(target, contents, name, enabled)
        }
    }
}

fn validate_skill_name(name: &str) -> Result<(), SkillConfigError> {
    if name.is_empty()
        || name.trim() != name
        || name.len() > 256
        || name.chars().any(char::is_control)
    {
        return Err(SkillConfigError::InvalidName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Returns a conservative key for native controls that identify Skills by name.
pub fn skill_name_key(name: &str) -> Result<String, SkillConfigError> {
    validate_skill_name(name)?;
    Ok(name.nfkc().case_fold().nfkc().collect())
}

fn native_skill_names_equal(target: SkillConfigTarget, left: &str, right: &str) -> bool {
    match target {
        SkillConfigTarget::GeminiSettings => left.to_lowercase() == right.to_lowercase(),
        SkillConfigTarget::GrokConfig | SkillConfigTarget::HermesConfig => left == right,
    }
}

fn native_skill_is_required(target: SkillConfigTarget, name: &str) -> bool {
    target == SkillConfigTarget::HermesConfig && name == "hermes-agent"
}

fn json_skills_object(
    target: SkillConfigTarget,
    root: &Value,
) -> Result<Option<&Map<String, Value>>, SkillConfigError> {
    let Some(skills) = root.get("skills") else {
        return Ok(None);
    };
    let skills = skills
        .as_object()
        .ok_or_else(|| invalid_skill_config(target, "'skills' must be an object"))?;
    Ok(Some(skills))
}

fn json_skills_enabled(target: SkillConfigTarget, root: &Value) -> Result<bool, SkillConfigError> {
    let Some(skills) = json_skills_object(target, root)? else {
        return Ok(true);
    };
    match skills.get("enabled") {
        None => Ok(true),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid_skill_config(target, "'skills.enabled' must be a boolean")),
    }
}

fn json_disabled_names(
    target: SkillConfigTarget,
    root: &Value,
) -> Result<Vec<String>, SkillConfigError> {
    let Some(skills) = json_skills_object(target, root)? else {
        return Ok(Vec::new());
    };
    let Some(disabled) = skills.get("disabled") else {
        return Ok(Vec::new());
    };
    string_array(target, disabled, "'skills.disabled'")
}

fn project_json_skill_enabled(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigError> {
    ensure_json_skill_control_writable(target, contents)?;
    let mut root = parse_skill_json(target, contents)?;
    let skills_enabled = json_skills_enabled(target, &root)?;
    if enabled && !skills_enabled {
        return Err(SkillConfigError::GloballyDisabled { target });
    }
    let disabled = json_disabled_names(target, &root)?;
    let explicitly_disabled = disabled
        .iter()
        .any(|entry| native_skill_names_equal(target, entry, name));
    if (enabled && !explicitly_disabled) || (!enabled && explicitly_disabled) {
        return Ok(None);
    }

    let root_object = root
        .as_object_mut()
        .expect("validated Skill JSON has an object root");
    let skills = root_object
        .entry("skills")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated skills entry is an object");
    let mut next = disabled
        .into_iter()
        .filter(|entry| !enabled || !native_skill_names_equal(target, entry, name))
        .collect::<Vec<_>>();
    if !enabled {
        next.push(name.to_owned());
    }
    skills.insert(
        "disabled".to_owned(),
        Value::Array(next.into_iter().map(Value::String).collect()),
    );

    let output = if let Some(contents) = contents.filter(|contents| !contents.is_empty()) {
        let original = std::str::from_utf8(contents)
            .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?;
        crate::json5_patch::replace_object_path_value(
            original,
            &["skills", "disabled"],
            root.get("skills")
                .and_then(Value::as_object)
                .and_then(|skills| skills.get("disabled"))
                .expect("projected JSON contains skills.disabled"),
        )
        .map_err(|message| invalid_skill_config(target, &message))?
    } else {
        let mut rendered = serde_json::to_string_pretty(&root)
            .map_err(|error| invalid_skill_config(target, &error.to_string()))?;
        rendered.push('\n');
        rendered
    };
    validate_projected_size(target, output)
}

fn ensure_json_skill_control_writable(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<(), SkillConfigError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(());
    };
    if contents.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_skill_config(target, "document is too large"));
    }
    let original = std::str::from_utf8(contents)
        .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?;
    let has_comments =
        crate::json5_patch::object_path_has_comments(original, &["skills", "disabled"])
            .map_err(|message| invalid_skill_config(target, &message))?;
    if has_comments {
        return Err(invalid_skill_config(
            target,
            "'skills.disabled' contains comments that cannot be preserved safely",
        ));
    }
    Ok(())
}

fn parse_skill_json(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<Value, SkillConfigError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(Value::Object(Map::new()));
    };
    if contents.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_skill_config(target, "document is too large"));
    }
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?;
    let root = json5::from_str::<Value>(text)
        .map_err(|_| invalid_skill_config(target, "document is not valid JSON settings"))?;
    if !root.is_object() {
        return Err(invalid_skill_config(target, "root must be an object"));
    }
    Ok(root)
}

fn string_array(
    target: SkillConfigTarget,
    value: &Value,
    label: &str,
) -> Result<Vec<String>, SkillConfigError> {
    value
        .as_array()
        .ok_or_else(|| invalid_skill_config(target, &format!("{label} must be an array")))?
        .iter()
        .map(|entry| {
            entry.as_str().map(str::to_owned).ok_or_else(|| {
                invalid_skill_config(target, &format!("{label} must contain strings"))
            })
        })
        .collect()
}

fn toml_disabled_names(
    target: SkillConfigTarget,
    document: &DocumentMut,
) -> Result<Vec<String>, SkillConfigError> {
    let Some(skills) = document.get("skills") else {
        return Ok(Vec::new());
    };
    let skills = skills
        .as_table_like()
        .ok_or_else(|| invalid_skill_config(target, "'skills' must be a table"))?;
    let Some(disabled) = skills.get("disabled") else {
        return Ok(Vec::new());
    };
    let disabled = disabled
        .as_array()
        .ok_or_else(|| invalid_skill_config(target, "'skills.disabled' must be an array"))?;
    if toml_array_has_comments(disabled) {
        return Err(invalid_skill_config(
            target,
            "'skills.disabled' contains comments that cannot be preserved safely",
        ));
    }
    disabled
        .iter()
        .map(|entry| {
            entry.as_str().map(str::to_owned).ok_or_else(|| {
                invalid_skill_config(target, "'skills.disabled' must contain strings")
            })
        })
        .collect()
}

fn toml_array_has_comments(array: &Array) -> bool {
    toml_raw_has_comment(array.trailing())
        || array
            .iter()
            .any(|value| toml_decor_has_comment(value.decor()))
}

fn toml_decor_has_comment(decor: &toml_edit::Decor) -> bool {
    [decor.prefix(), decor.suffix()]
        .into_iter()
        .flatten()
        .any(toml_raw_has_comment)
}

fn toml_raw_has_comment(raw: &toml_edit::RawString) -> bool {
    raw.as_str().is_none_or(|raw| raw.contains('#'))
}

fn project_toml_skill_enabled(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigError> {
    let mut document = parse_skill_toml(target, contents)?;
    let disabled = toml_disabled_names(target, &document)?;
    let currently_enabled = !disabled.iter().any(|entry| entry == name);
    if currently_enabled == enabled {
        return Ok(None);
    }
    if document.get("skills").is_none() {
        document["skills"] = Item::Table(Table::new());
    }
    let skills = document["skills"]
        .as_table_like_mut()
        .expect("validated skills entry is a table");
    if skills.get("disabled").is_none() {
        skills.insert("disabled", Item::Value(Array::new().into()));
    }
    let array = skills
        .get_mut("disabled")
        .and_then(Item::as_array_mut)
        .expect("validated disabled entry is an array");
    if enabled {
        for index in (0..array.len()).rev() {
            if array.get(index).and_then(toml_edit::Value::as_str) == Some(name) {
                array.remove(index);
            }
        }
    } else {
        array.push(name);
    }
    validate_projected_size(target, document.to_string())
}

fn parse_skill_toml(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<DocumentMut, SkillConfigError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(DocumentMut::new());
    };
    if contents.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_skill_config(target, "document is too large"));
    }
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?;
    text.parse::<DocumentMut>()
        .map_err(|_| invalid_skill_config(target, "document is not valid TOML"))
}

fn yaml_skills_mapping(
    target: SkillConfigTarget,
    root: &serde_yaml::Value,
) -> Result<Option<&serde_yaml::Mapping>, SkillConfigError> {
    let root = root
        .as_mapping()
        .ok_or_else(|| invalid_skill_config(target, "root must be a mapping"))?;
    let key = serde_yaml::Value::String("skills".to_owned());
    match root.get(&key) {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(skills) => skills
            .as_mapping()
            .map(Some)
            .ok_or_else(|| invalid_skill_config(target, "'skills' must be a mapping")),
    }
}

fn yaml_disabled_names(
    target: SkillConfigTarget,
    root: &serde_yaml::Value,
) -> Result<Vec<String>, SkillConfigError> {
    let Some(skills) = yaml_skills_mapping(target, root)? else {
        return Ok(Vec::new());
    };
    let key = serde_yaml::Value::String("disabled".to_owned());
    match skills.get(&key) {
        None | Some(serde_yaml::Value::Null) => Ok(Vec::new()),
        Some(value) => yaml_name_list(target, value, "'skills.disabled'"),
    }
}

fn yaml_platform_disables_name(
    target: SkillConfigTarget,
    root: &serde_yaml::Value,
    name: &str,
) -> Result<bool, SkillConfigError> {
    let Some(skills) = yaml_skills_mapping(target, root)? else {
        return Ok(false);
    };
    let key = serde_yaml::Value::String("platform_disabled".to_owned());
    let Some(platforms) = skills.get(&key) else {
        return Ok(false);
    };
    if platforms.is_null() {
        return Ok(false);
    }
    let platforms = platforms.as_mapping().ok_or_else(|| {
        invalid_skill_config(target, "'skills.platform_disabled' must be a mapping")
    })?;
    for value in platforms.values() {
        if yaml_name_list(target, value, "'skills.platform_disabled.*'")?
            .iter()
            .any(|entry| entry == name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn yaml_name_list(
    target: SkillConfigTarget,
    value: &serde_yaml::Value,
    label: &str,
) -> Result<Vec<String>, SkillConfigError> {
    let values = match value {
        serde_yaml::Value::Null => return Ok(Vec::new()),
        serde_yaml::Value::String(value) => return Ok(trimmed_yaml_name(value)),
        serde_yaml::Value::Sequence(values) => values,
        _ => {
            return Err(invalid_skill_config(
                target,
                &format!("{label} must be a string or string array"),
            ))
        }
    };
    let mut names = Vec::new();
    for value in values {
        let value = value.as_str().ok_or_else(|| {
            invalid_skill_config(target, &format!("{label} must contain strings"))
        })?;
        names.extend(trimmed_yaml_name(value));
    }
    Ok(names)
}

fn trimmed_yaml_name(value: &str) -> Vec<String> {
    let value = value.trim();
    (!value.is_empty())
        .then(|| value.to_owned())
        .into_iter()
        .collect()
}

fn project_yaml_skill_enabled(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigError> {
    let mut root = parse_skill_yaml(target, contents)?;
    let skills_key = serde_yaml::Value::String("skills".to_owned());
    let section_existed = root
        .as_mapping()
        .expect("validated Skill YAML has a mapping root")
        .contains_key(&skills_key);
    let platform_disabled = yaml_platform_disables_name(target, &root, name)?;
    if enabled && platform_disabled {
        return Err(SkillConfigError::ExternallyDisabled { target });
    }
    let disabled = yaml_disabled_names(target, &root)?;
    let explicitly_disabled = disabled.iter().any(|entry| entry == name);
    if (enabled && !explicitly_disabled) || (!enabled && explicitly_disabled) {
        return Ok(None);
    }

    let root_mapping = root
        .as_mapping_mut()
        .expect("validated Skill YAML has a mapping root");
    if !root_mapping
        .get(&skills_key)
        .is_some_and(serde_yaml::Value::is_mapping)
    {
        root_mapping.insert(
            skills_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let skills = root_mapping
        .get_mut(&skills_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .expect("projected skills entry is a mapping");
    let mut next = disabled
        .into_iter()
        .filter(|entry| !enabled || entry != name)
        .collect::<Vec<_>>();
    if !enabled {
        next.push(name.to_owned());
    }
    skills.insert(
        serde_yaml::Value::String("disabled".to_owned()),
        serde_yaml::Value::Sequence(next.into_iter().map(serde_yaml::Value::String).collect()),
    );
    let original = contents
        .filter(|contents| !contents.is_empty())
        .map(std::str::from_utf8)
        .transpose()
        .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?
        .unwrap_or_default();
    let output = crate::yaml_patch::replace_top_level_section(
        original,
        "skills",
        root_mapping
            .get(&skills_key)
            .expect("projected YAML contains skills"),
        section_existed,
    )
    .map_err(|message| invalid_skill_config(target, &message))?;
    serde_yaml::from_str::<serde_yaml::Value>(&output).map_err(|_| {
        invalid_skill_config(
            target,
            "projected skills section would invalidate the YAML document",
        )
    })?;
    validate_projected_size(target, output)
}

fn parse_skill_yaml(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<serde_yaml::Value, SkillConfigError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    };
    if contents.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_skill_config(target, "document is too large"));
    }
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid_skill_config(target, "document is not UTF-8"))?;
    if crate::yaml_patch::top_level_section_has_comments(text, "skills") {
        return Err(invalid_skill_config(
            target,
            "the 'skills' section contains comments that cannot be preserved safely",
        ));
    }
    if crate::yaml_patch::top_level_section_has_references(text, "skills") {
        return Err(invalid_skill_config(
            target,
            "the 'skills' section contains YAML anchors, aliases, or merge keys that cannot be preserved safely",
        ));
    }
    let root = serde_yaml::from_str::<serde_yaml::Value>(text)
        .map_err(|_| invalid_skill_config(target, "document is not valid YAML"))?;
    if !root.is_mapping() {
        return Err(invalid_skill_config(target, "root must be a mapping"));
    }
    Ok(root)
}

fn validate_projected_size(
    target: SkillConfigTarget,
    output: String,
) -> Result<Option<String>, SkillConfigError> {
    if output.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_skill_config(
            target,
            "projected document is too large",
        ));
    }
    Ok(Some(output))
}

fn invalid_skill_config(target: SkillConfigTarget, message: &str) -> SkillConfigError {
    SkillConfigError::InvalidConfig {
        target,
        message: message.to_owned(),
    }
}

/// Applies an enable or disable operation and returns a reversible receipt.
pub fn apply_skill_deployment(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
    enabled: bool,
    sync_method: SkillSyncMethod,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    apply_skill_deployment_with_policy(
        source_root,
        destination_root,
        directory,
        enabled,
        sync_method,
        SkillCopyPolicy::ManagedOnly,
    )
}

/// Applies a deployment using host-supplied evidence for legacy copied directories.
pub fn apply_skill_deployment_with_policy(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
    enabled: bool,
    sync_method: SkillSyncMethod,
    copy_policy: SkillCopyPolicy,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let paths = deployment_paths(source_root, destination_root, directory)?;
    recover_interrupted_deployment(&paths, directory, enabled, copy_policy)?;
    let (state, expectation) =
        inspect_paths_with_policy(&paths.source, &paths.destination, directory, copy_policy)?;
    if enabled {
        return match state {
            SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
                Ok(observed_receipt(paths.destination, expectation))
            }
            SkillDeploymentState::Missing => {
                enable_deployment(paths, directory, sync_method, copy_policy)
            }
        };
    }

    match state {
        SkillDeploymentState::Missing => Ok(observed_receipt(paths.destination, expectation)),
        SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
            disable_deployment(paths, directory, expectation)
        }
    }
}

struct DeploymentPaths {
    source_root: PathBuf,
    destination_root: PathBuf,
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
        source_root: source_root.to_owned(),
        destination_root: destination_root.to_owned(),
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
    let ownership_marker = source.join(MANAGED_COPY_MARKER_FILE);
    match fs::symlink_metadata(&ownership_marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(SkillConfigError::UnsupportedEntry {
                path: ownership_marker,
            })
        }
        Err(source) => return Err(SkillConfigError::io(&ownership_marker, source)),
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
    let (resolved, suffix) = canonicalized_ancestor(path)?;
    let normalized = append_normalized_suffix(resolved, &suffix, path)?;

    // Normalizing a missing `segment/..` pair can reveal an existing symlink.
    // Resolve once more so callers compare the final filesystem identity.
    let (resolved, suffix) = canonicalized_ancestor(&normalized)?;
    append_normalized_suffix(resolved, &suffix, path)
}

fn canonicalized_ancestor(path: &Path) -> Result<(PathBuf, PathBuf), SkillConfigError> {
    let mut last_missing = None;
    for ancestor in path.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(resolved) => {
                let suffix =
                    path.strip_prefix(ancestor)
                        .map_err(|_| SkillConfigError::RelativeRoot {
                            path: path.to_owned(),
                        })?;
                return Ok((resolved, suffix.to_owned()));
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                last_missing = Some((ancestor.to_owned(), source));
            }
            Err(source) => return Err(SkillConfigError::io(ancestor, source)),
        }
    }
    let (ancestor, source) = last_missing.ok_or_else(|| SkillConfigError::RelativeRoot {
        path: path.to_owned(),
    })?;
    Err(SkillConfigError::io(ancestor, source))
}

fn append_normalized_suffix(
    mut resolved: PathBuf,
    suffix: &Path,
    original: &Path,
) -> Result<PathBuf, SkillConfigError> {
    for component in suffix.components() {
        match component {
            Component::Normal(name) => resolved.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(SkillConfigError::RelativeRoot {
                        path: original.to_owned(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(SkillConfigError::RelativeRoot {
                    path: original.to_owned(),
                })
            }
        }
    }
    Ok(resolved)
}

fn inspect_paths_with_policy(
    source: &Path,
    destination: &Path,
    directory: &str,
    copy_policy: SkillCopyPolicy,
) -> Result<(SkillDeploymentState, DeploymentExpectation), SkillConfigError> {
    let link_parent = destination
        .parent()
        .expect("a deployment has a destination root");
    inspect_paths_with_link_parent_policy(source, destination, directory, link_parent, copy_policy)
}

fn inspect_paths_with_link_parent_policy(
    source: &Path,
    destination: &Path,
    directory: &str,
    link_parent: &Path,
    copy_policy: SkillCopyPolicy,
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
            link_parent.join(&target)
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
        if let Some(digest) = managed_copy_digest_if_owned(destination, directory)? {
            return Ok((
                SkillDeploymentState::Copied,
                DeploymentExpectation::Copied {
                    digest,
                    directory: directory.to_owned(),
                },
            ));
        }
        if copy_policy == SkillCopyPolicy::AllowMatching {
            let source_digest = tree_digest(source)?;
            let destination_digest = tree_digest(destination)?;
            if source_digest == destination_digest {
                return Ok((
                    SkillDeploymentState::Copied,
                    DeploymentExpectation::MatchingCopy {
                        digest: destination_digest,
                    },
                ));
            }
        }
    }

    Err(SkillConfigError::Conflict {
        path: destination.to_owned(),
    })
}

fn enable_deployment(
    paths: DeploymentPaths,
    directory: &str,
    sync_method: SkillSyncMethod,
    copy_policy: SkillCopyPolicy,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root")
        .to_owned();
    crate::fs::create_dir_all_durable(&parent)
        .map_err(|source| SkillConfigError::io(&parent, source))?;
    ensure_distinct_roots(&paths.source_root, &paths.destination_root)?;

    if !matches!(sync_method, SkillSyncMethod::Copy) {
        match create_link_deployment(&paths, directory) {
            Ok(receipt) => return Ok(receipt),
            Err(error)
                if matches!(sync_method, SkillSyncMethod::Symlink)
                    || matches!(&error, SkillConfigError::Recovery { .. }) =>
            {
                return Err(error)
            }
            Err(_) => match inspect_paths_with_policy(
                &paths.source,
                &paths.destination,
                directory,
                copy_policy,
            )? {
                (SkillDeploymentState::Missing, _) => {}
                (_, expectation) => {
                    return Ok(observed_receipt(paths.destination, expectation));
                }
            },
        }
    }

    create_copy(paths, directory)
}

fn create_link_deployment(
    paths: &DeploymentPaths,
    directory: &str,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root =
        create_temporary_directory(parent, directory, InterruptedOperation::Enable)?;
    let staged = temporary_root.join("deployment");
    if let Err(error) = create_symlink(&paths.source, &staged) {
        return Err(cleanup_temporary(&temporary_root, error));
    }
    if let Err(error) = rename_path(&staged, &paths.destination) {
        return Err(cleanup_temporary(&temporary_root, error));
    }
    let receipt = SkillDeploymentReceipt {
        change: DeploymentChange::Created {
            destination: paths.destination.clone(),
            expectation: DeploymentExpectation::Linked {
                target: paths.source.clone(),
            },
        },
    };
    if let Err(error) = remove_directory(&temporary_root) {
        return Err(recover_created(receipt, error));
    }
    if let Err(error) = receipt.verify() {
        return Err(recover_created(receipt, error));
    }
    Ok(receipt)
}

fn create_copy(
    paths: DeploymentPaths,
    directory: &str,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root =
        create_temporary_directory(parent, directory, InterruptedOperation::Enable)?;
    let staged = temporary_root.join("deployment");
    let staged_digest = (|| {
        let mut budget = TreeBudget::default();
        copy_tree(&paths.source, &staged, &mut budget)?;
        let source_digest = tree_digest(&paths.source)?;
        let staged_digest = tree_digest(&staged)?;
        if source_digest == staged_digest {
            write_managed_copy_marker(&staged, directory, &source_digest)?;
        }
        Ok((source_digest, staged_digest))
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
    if let Err(error) = rename_path(&staged, &paths.destination) {
        return Err(cleanup_temporary(&temporary_root, error));
    }
    let receipt = SkillDeploymentReceipt {
        change: DeploymentChange::Created {
            destination: paths.destination,
            expectation: DeploymentExpectation::Copied {
                digest: source_digest,
                directory: directory.to_owned(),
            },
        },
    };
    if let Err(error) = remove_directory(&temporary_root) {
        return Err(recover_created(receipt, error));
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
    paths: DeploymentPaths,
    directory: &str,
    expectation: DeploymentExpectation,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    ensure_distinct_roots(&paths.source_root, &paths.destination_root)?;
    require_expectation(&paths.destination, &expectation)?;
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root =
        create_temporary_directory(parent, directory, InterruptedOperation::Disable)?;
    let backup = temporary_root.join("deployment");
    if let Err(error) = ensure_distinct_roots(&paths.source_root, &paths.destination_root)
        .and_then(|()| require_expectation(&paths.destination, &expectation))
    {
        return Err(cleanup_temporary(&temporary_root, error));
    }
    if let Err(error) = rename_path(&paths.destination, &backup) {
        return Err(cleanup_temporary(&temporary_root, error));
    }
    let receipt = SkillDeploymentReceipt {
        change: DeploymentChange::Removed {
            destination: paths.destination,
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
    if matches!(&error, SkillConfigError::Recovery { .. }) {
        return error;
    }
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
        DeploymentExpectation::Copied { digest, directory } => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                managed_copy_matches(path, directory, digest)?
            }
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => return Err(SkillConfigError::io(path, source)),
        },
        DeploymentExpectation::MatchingCopy { digest } => match fs::symlink_metadata(path) {
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
    tree_digest_with_managed_marker(root, false)
}

fn managed_copy_digest(root: &Path) -> Result<String, SkillConfigError> {
    tree_digest_with_managed_marker(root, true)
}

fn tree_digest_with_managed_marker(
    root: &Path,
    skip_managed_marker: bool,
) -> Result<String, SkillConfigError> {
    let metadata =
        fs::symlink_metadata(root).map_err(|source| SkillConfigError::io(root, source))?;
    if !metadata.file_type().is_dir() {
        return Err(SkillConfigError::UnsupportedEntry {
            path: root.to_owned(),
        });
    }
    let mut budget = TreeBudget::default();
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-skill-tree-v1\0");
    hash_directory(root, root, &mut budget, &mut hasher, skip_managed_marker)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn managed_copy_matches(
    destination: &Path,
    directory: &str,
    expected_digest: &str,
) -> Result<bool, SkillConfigError> {
    Ok(managed_copy_digest_if_owned(destination, directory)?.as_deref() == Some(expected_digest))
}

fn managed_copy_digest_if_owned(
    destination: &Path,
    directory: &str,
) -> Result<Option<String>, SkillConfigError> {
    let Some(marker) = read_managed_copy_marker(destination)? else {
        return Ok(None);
    };
    if marker.version != MANAGED_COPY_MARKER_VERSION || marker.directory != directory {
        return Ok(None);
    }
    if managed_copy_digest(destination)? != marker.source_digest {
        return Ok(None);
    }
    Ok(Some(marker.source_digest))
}

fn read_managed_copy_marker(
    destination: &Path,
) -> Result<Option<ManagedCopyMarker>, SkillConfigError> {
    let path = destination.join(MANAGED_COPY_MARKER_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.len() > MAX_MANAGED_COPY_MARKER_BYTES {
        return Ok(None);
    }
    let contents = fs::read(&path).map_err(|source| SkillConfigError::io(&path, source))?;
    Ok(serde_json::from_slice(&contents).ok())
}

fn write_managed_copy_marker(
    destination: &Path,
    directory: &str,
    source_digest: &str,
) -> Result<(), SkillConfigError> {
    let path = destination.join(MANAGED_COPY_MARKER_FILE);
    let contents = serde_json::to_vec(&ManagedCopyMarker {
        version: MANAGED_COPY_MARKER_VERSION,
        directory: directory.to_owned(),
        source_digest: source_digest.to_owned(),
    })
    .map_err(|error| SkillConfigError::Recovery {
        message: format!("failed to encode managed Skill marker: {error}"),
    })?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|source| SkillConfigError::io(&path, source))?;
    file.write_all(&contents)
        .and_then(|()| file.sync_all())
        .map_err(|source| SkillConfigError::io(&path, source))?;
    sync_directory(destination)
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
    skip_managed_marker: bool,
) -> Result<(), SkillConfigError> {
    let entries = read_directory_entries(directory, budget, skip_managed_marker)?;

    for entry in entries {
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
            hash_directory(root, &path, budget, hasher, false)?;
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
    hasher.update(before.len().to_le_bytes());
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
    let entries = read_directory_entries(source, budget, false)?;
    for entry in entries {
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
            File::open(&destination_path)
                .and_then(|file| file.sync_all())
                .map_err(|source_error| SkillConfigError::io(&destination_path, source_error))?;
        } else {
            return Err(SkillConfigError::UnsupportedEntry { path: source_path });
        }
    }
    sync_directory(destination)
}

fn read_directory_entries(
    directory: &Path,
    budget: &mut TreeBudget,
    skip_managed_marker: bool,
) -> Result<Vec<DirEntry>, SkillConfigError> {
    let reader =
        fs::read_dir(directory).map_err(|source| SkillConfigError::io(directory, source))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|source| SkillConfigError::io(directory, source))?;
        if skip_managed_marker && entry.file_name() == MANAGED_COPY_MARKER_FILE {
            continue;
        }
        budget.add_entry()?;
        entries.push(entry);
    }
    entries.sort_by_key(DirEntry::file_name);
    Ok(entries)
}

fn recover_interrupted_deployment(
    paths: &DeploymentPaths,
    directory: &str,
    enabled: bool,
    copy_policy: SkillCopyPolicy,
) -> Result<(), SkillConfigError> {
    let parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SkillConfigError::io(parent, source)),
    };
    let mut interrupted = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_DIRECTORY_PREFIX))
        {
            continue;
        }
        let temporary_root = entry.path();
        let metadata = fs::symlink_metadata(&temporary_root)
            .map_err(|source| SkillConfigError::io(&temporary_root, source))?;
        if !metadata.file_type().is_dir() {
            continue;
        }
        let tagged_for_directory = temporary_name_matches_directory(&name, directory);
        let marker = match read_operation_marker(&temporary_root) {
            Ok(Some(marker)) => marker,
            Ok(None) if tagged_for_directory => {
                if remove_empty_interrupted_operation(&temporary_root)? {
                    continue;
                }
                recover_unidentified_operation(&temporary_root, directory, "missing or invalid")?;
                continue;
            }
            Err(error) if tagged_for_directory => {
                recover_unidentified_operation(&temporary_root, directory, &error.to_string())?;
                continue;
            }
            Ok(None) | Err(_) => continue,
        };
        if marker.directory != directory {
            if tagged_for_directory {
                return Err(SkillConfigError::Recovery {
                    message: format!(
                        "Skill operation marker at {:?} names a different directory",
                        temporary_root
                    ),
                });
            }
            continue;
        }
        if marker.version != TEMP_MARKER_VERSION {
            return Err(SkillConfigError::Recovery {
                message: format!(
                    "unsupported Skill operation marker version {} at {:?}",
                    marker.version, temporary_root
                ),
            });
        }
        validate_skill_directory(&marker.directory)?;
        interrupted.push((temporary_root, marker.operation));
    }
    interrupted.sort_by(|left, right| left.0.cmp(&right.0));
    for (temporary_root, operation) in interrupted {
        match operation {
            InterruptedOperation::Enable => {
                recover_interrupted_enable(paths, &temporary_root, directory, enabled, copy_policy)?
            }
            InterruptedOperation::Disable => recover_interrupted_disable(
                paths,
                &temporary_root,
                directory,
                enabled,
                copy_policy,
            )?,
        }
    }
    Ok(())
}

fn remove_empty_interrupted_operation(path: &Path) -> Result<bool, SkillConfigError> {
    let mut entries = fs::read_dir(path).map_err(|source| SkillConfigError::io(path, source))?;
    match entries.next() {
        Some(Ok(_)) => return Ok(false),
        Some(Err(source)) => return Err(SkillConfigError::io(path, source)),
        None => {}
    }
    match fs::remove_dir(path) {
        Ok(()) => {
            #[cfg(not(windows))]
            if let Some(parent) = path.parent() {
                sync_directory(parent)
                    .map_err(|error| post_visible_error("empty operation cleanup", error))?;
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(source) => Err(SkillConfigError::io(path, source)),
    }
}

fn recover_unidentified_operation(
    temporary_root: &Path,
    directory: &str,
    marker_issue: &str,
) -> Result<(), SkillConfigError> {
    Err(SkillConfigError::Recovery {
        message: format!(
            "cannot identify interrupted Skill operation for '{directory}' at {temporary_root:?}: {marker_issue}"
        ),
    })
}

fn read_operation_marker(
    temporary_root: &Path,
) -> Result<Option<InterruptedOperationMarker>, SkillConfigError> {
    let path = temporary_root.join(TEMP_MARKER_FILE);
    match read_operation_marker_file(&path)? {
        OperationMarkerFile::Valid(marker) => return Ok(Some(marker)),
        OperationMarkerFile::Invalid => return Ok(None),
        OperationMarkerFile::Missing => {}
    }

    let staging = temporary_root.join(TEMP_MARKER_STAGING_FILE);
    match read_operation_marker_file(&staging)? {
        OperationMarkerFile::Valid(marker) => {
            rename_path(&staging, &path)?;
            Ok(Some(marker))
        }
        OperationMarkerFile::Missing | OperationMarkerFile::Invalid => Ok(None),
    }
}

fn read_operation_marker_file(path: &Path) -> Result<OperationMarkerFile, SkillConfigError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(OperationMarkerFile::Invalid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OperationMarkerFile::Missing)
        }
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.len() > MAX_TEMP_MARKER_BYTES {
        return Ok(OperationMarkerFile::Invalid);
    }
    let contents = fs::read(path).map_err(|source| SkillConfigError::io(path, source))?;
    Ok(serde_json::from_slice(&contents)
        .map(OperationMarkerFile::Valid)
        .unwrap_or(OperationMarkerFile::Invalid))
}

fn recover_interrupted_enable(
    paths: &DeploymentPaths,
    temporary_root: &Path,
    directory: &str,
    enabled: bool,
    copy_policy: SkillCopyPolicy,
) -> Result<(), SkillConfigError> {
    let (destination_state, _) =
        inspect_paths_with_policy(&paths.source, &paths.destination, directory, copy_policy)?;
    let staged = temporary_root.join("deployment");
    match inspect_paths_with_policy(&paths.source, &staged, directory, copy_policy) {
        Ok((SkillDeploymentState::Linked, _)) => {
            if enabled && destination_state == SkillDeploymentState::Missing {
                rename_path(&staged, &paths.destination)?;
            }
        }
        Ok((SkillDeploymentState::Copied, expectation)) => {
            let staged_digest = match expectation {
                DeploymentExpectation::Copied { digest, .. }
                | DeploymentExpectation::MatchingCopy { digest } => digest,
                _ => {
                    return Err(SkillConfigError::Recovery {
                        message: format!(
                            "interrupted Skill enable has an invalid copied stage at {staged:?}"
                        ),
                    })
                }
            };
            if tree_digest(&paths.source)? != staged_digest {
                // The source changed after this copy was staged. Discard the
                // Core-owned stage so the caller rebuilds it from the current source.
                return remove_directory(temporary_root);
            }
            if enabled && destination_state == SkillDeploymentState::Missing {
                rename_path(&staged, &paths.destination)?;
            }
        }
        Ok((SkillDeploymentState::Missing, _)) => {}
        Err(SkillConfigError::Conflict { .. }) => {
            // A valid operation marker establishes that the stage is Core-owned
            // scratch space. An incomplete copy can be discarded and rebuilt.
            return remove_directory(temporary_root);
        }
        Err(error) => return Err(error),
    }
    remove_directory(temporary_root)
}

fn recover_interrupted_disable(
    paths: &DeploymentPaths,
    temporary_root: &Path,
    directory: &str,
    enabled: bool,
    copy_policy: SkillCopyPolicy,
) -> Result<(), SkillConfigError> {
    let backup = temporary_root.join("deployment");
    let (destination_state, _) =
        inspect_paths_with_policy(&paths.source, &paths.destination, directory, copy_policy)?;
    let original_parent = paths
        .destination
        .parent()
        .expect("a deployment has a destination root");
    let (backup_state, _) = match inspect_paths_with_link_parent_policy(
        &paths.source,
        &backup,
        directory,
        original_parent,
        copy_policy,
    ) {
        Ok(state) => state,
        Err(SkillConfigError::Conflict { .. }) => {
            return Err(SkillConfigError::Recovery {
                message: format!(
                    "interrupted Skill disable has an unverified backup at {backup:?}"
                ),
            });
        }
        Err(error) => return Err(error),
    };
    if destination_state == SkillDeploymentState::Missing {
        if enabled && backup_state != SkillDeploymentState::Missing {
            rename_path(&backup, &paths.destination)?;
        }
        return remove_directory(temporary_root);
    }
    match backup_state {
        SkillDeploymentState::Missing => remove_directory(temporary_root),
        SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
            Err(SkillConfigError::Recovery {
                message: format!(
                    "interrupted Skill disable has both destination and backup at {:?}",
                    paths.destination
                ),
            })
        }
    }
}

fn create_temporary_directory(
    parent: &Path,
    directory: &str,
    operation: InterruptedOperation,
) -> Result<PathBuf, SkillConfigError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut last_error = None;
    let directory_tag = operation_directory_tag(directory);
    for _ in 0..16 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "{TEMP_DIRECTORY_PREFIX}{directory_tag}.{}.{timestamp}.{counter}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                let marker = InterruptedOperationMarker {
                    version: TEMP_MARKER_VERSION,
                    operation,
                    directory: directory.to_owned(),
                };
                let result = write_operation_marker(&path, &marker);
                return match result {
                    Ok(()) => match sync_directory(parent) {
                        Ok(()) => Ok(path),
                        Err(error) => Err(cleanup_temporary(&path, error)),
                    },
                    Err(error) => Err(cleanup_temporary(&path, error)),
                };
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some((path, source));
            }
            Err(source) => return Err(SkillConfigError::io(path, source)),
        }
    }
    let (path, source) = last_error.expect("temporary directory loop must run");
    Err(SkillConfigError::io(path, source))
}

fn temporary_name_matches_directory(name: &std::ffi::OsStr, directory: &str) -> bool {
    name.to_str().is_some_and(|name| {
        name.starts_with(&format!(
            "{TEMP_DIRECTORY_PREFIX}{}.",
            operation_directory_tag(directory)
        ))
    })
}

fn operation_directory_tag(directory: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(directory.as_bytes());
    let mut tag = String::with_capacity(16);
    for byte in &digest[..8] {
        tag.push(char::from(HEX[usize::from(byte >> 4)]));
        tag.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    tag
}

fn write_operation_marker(
    temporary_root: &Path,
    marker: &InterruptedOperationMarker,
) -> Result<(), SkillConfigError> {
    let path = temporary_root.join(TEMP_MARKER_FILE);
    let staging = temporary_root.join(TEMP_MARKER_STAGING_FILE);
    let contents = serde_json::to_vec(marker).map_err(|error| SkillConfigError::Recovery {
        message: format!("failed to encode Skill operation marker: {error}"),
    })?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging)
        .map_err(|source| SkillConfigError::io(&staging, source))?;
    if let Err(source) = file.write_all(&contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staging);
        return Err(SkillConfigError::io(&staging, source));
    }
    drop(file);
    if let Err(error) = rename_path(&staging, &path) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SkillConfigError> {
    crate::fs::sync_directory(path).map_err(|source| SkillConfigError::io(path, source))
}

fn rename_path(source: &Path, destination: &Path) -> Result<(), SkillConfigError> {
    #[cfg(windows)]
    crate::fs::move_path_write_through(source, destination)
        .map_err(|error| SkillConfigError::io(destination, error))?;
    #[cfg(not(windows))]
    fs::rename(source, destination).map_err(|error| SkillConfigError::io(destination, error))?;

    #[cfg(not(windows))]
    if let Some(parent) = source.parent() {
        sync_directory(parent).map_err(|error| post_visible_error("rename", error))?;
    }
    #[cfg(not(windows))]
    if destination.parent() != source.parent() {
        if let Some(parent) = destination.parent() {
            sync_directory(parent).map_err(|error| post_visible_error("rename", error))?;
        }
    }
    Ok(())
}

fn post_visible_error(operation: &str, error: SkillConfigError) -> SkillConfigError {
    SkillConfigError::Recovery {
        message: format!("{operation} completed but could not be made durable: {error}"),
    }
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

#[cfg(not(windows))]
fn remove_deployment(path: &Path) -> Result<(), SkillConfigError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.file_type().is_symlink() {
        remove_directory_symlink(path)?;
    } else if metadata.file_type().is_file() {
        fs::remove_file(path).map_err(|source| SkillConfigError::io(path, source))?;
    } else if metadata.file_type().is_dir() {
        return remove_directory(path);
    } else {
        return Err(SkillConfigError::UnsupportedEntry {
            path: path.to_owned(),
        });
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent).map_err(|error| post_visible_error("removal", error))?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_deployment(path: &Path) -> Result<(), SkillConfigError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return remove_directory(path);
    }
    if !metadata.file_type().is_symlink() && !metadata.file_type().is_file() {
        return Err(SkillConfigError::UnsupportedEntry {
            path: path.to_owned(),
        });
    }
    let tombstone = crate::fs::move_path_to_tombstone_write_through(path)
        .map_err(|source| SkillConfigError::io(path, source))?;
    let removed = if metadata.file_type().is_symlink() {
        remove_directory_symlink(&tombstone)
    } else {
        fs::remove_file(&tombstone).map_err(|source| SkillConfigError::io(&tombstone, source))
    };
    removed.map_err(|error| post_visible_error("deployment removal", error))
}

#[cfg(unix)]
fn remove_directory_symlink(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_file(path).map_err(|source| SkillConfigError::io(path, source))
}

#[cfg(windows)]
fn remove_directory_symlink(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_dir(path).map_err(|source| SkillConfigError::io(path, source))
}

#[cfg(not(windows))]
fn remove_directory(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_dir_all(path).map_err(|source| SkillConfigError::io(path, source))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent).map_err(|error| post_visible_error("directory removal", error))?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_directory(path: &Path) -> Result<(), SkillConfigError> {
    let tombstone = crate::fs::move_path_to_tombstone_write_through(path)
        .map_err(|source| SkillConfigError::io(path, source))?;
    fs::remove_dir_all(&tombstone)
        .map_err(|source| SkillConfigError::io(&tombstone, source))
        .map_err(|error| post_visible_error("directory removal", error))
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

    #[cfg(unix)]
    #[test]
    fn post_rename_sync_failure_preserves_the_moved_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempdir().unwrap();
        let parent = temporary.path().join("skills");
        let source = parent.join("source");
        let destination = parent.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "# Skill\n").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();

        let result = rename_path(&source, &destination);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(result, Err(SkillConfigError::Recovery { .. })));
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "# Skill\n"
        );
    }

    #[test]
    fn uncertain_recovery_data_is_not_cleaned_up() {
        let temporary = tempdir().unwrap();
        let state = temporary.path().join("operation");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("deployment"), "preserve").unwrap();

        let error = cleanup_temporary(
            &state,
            SkillConfigError::Recovery {
                message: "state may be visible".to_owned(),
            },
        );

        assert!(matches!(error, SkillConfigError::Recovery { .. }));
        assert_eq!(fs::read(state.join("deployment")).unwrap(), b"preserve");
    }

    #[test]
    fn contracts_resolve_routes_and_effective_state() {
        let controlled = SkillAppContract::host_managed()
            .with_unified_store_discovery()
            .with_config_target(SkillConfigTarget::GeminiSettings);
        let route = resolve_skill_route(controlled, SkillDiscoveryState::Selected).unwrap();
        assert_eq!(
            route.config_target(),
            Some(SkillConfigTarget::GeminiSettings)
        );
        assert!(!route.deploy_native());
        assert_eq!(
            resolve_skill_effective_state(
                controlled,
                SkillDiscoveryState::Selected,
                Some(false),
                Some(SkillConfigState::Enabled)
            )
            .enabled(),
            Some(true)
        );
        let missing_required = resolve_skill_effective_state(
            controlled,
            SkillDiscoveryState::Selected,
            Some(false),
            Some(SkillConfigState::Required),
        );
        assert_eq!(missing_required.enabled(), Some(false));
        assert_eq!(missing_required.reason(), None);
        let present_required = resolve_skill_effective_state(
            controlled,
            SkillDiscoveryState::Selected,
            Some(true),
            Some(SkillConfigState::Required),
        );
        assert_eq!(present_required.enabled(), Some(true));
        assert_eq!(
            present_required.reason(),
            Some(SkillControlReason::Required)
        );

        let direct = SkillAppContract::host_managed().with_unified_store_discovery();
        assert_eq!(
            resolve_skill_route(direct, SkillDiscoveryState::Selected),
            Err(SkillControlReason::DirectUnifiedDiscovery)
        );
    }

    #[test]
    fn directory_names_are_single_normalized_components() {
        validate_skill_directory("docs").unwrap();
        for invalid in [
            "", " docs", "docs ", ".docs", "../docs", "a/b", "a\\b", "docs.", "CON", "nul.txt",
            "a:b",
        ] {
            assert!(validate_skill_directory(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            skill_directory_key("É").unwrap(),
            skill_directory_key("e\u{301}").unwrap()
        );
        assert_eq!(
            skill_directory_key("σ").unwrap(),
            skill_directory_key("ς").unwrap()
        );
        assert_eq!(
            skill_name_key("Ｄocs").unwrap(),
            skill_name_key("docs").unwrap()
        );
        for invalid in ["COM¹", "com².txt", "LPT³"] {
            assert!(validate_skill_directory(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn presence_is_independent_from_copy_ownership() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(destination.join("docs")).unwrap();
        fs::write(destination.join("docs/SKILL.md"), "different").unwrap();

        assert!(inspect_skill_presence(&destination, "docs").unwrap());
        assert!(matches!(
            inspect_skill_deployment(&source, &destination, "docs"),
            Err(SkillConfigError::Conflict { .. })
        ));
    }

    #[test]
    fn discovery_distinguishes_selected_and_external_skills() {
        let (_temporary, source, destination) = roots();
        assert_eq!(
            inspect_skill_discovery(&source, &destination, "docs").unwrap(),
            SkillDiscoveryState::Missing
        );

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            inspect_skill_discovery(&source, &destination, "docs").unwrap(),
            SkillDiscoveryState::Selected
        );

        fs::write(destination.join("docs/extra"), "external").unwrap();
        assert_eq!(
            inspect_skill_discovery(&source, &destination, "docs").unwrap(),
            SkillDiscoveryState::External
        );
    }

    #[test]
    fn discovery_accepts_the_catalog_as_the_direct_store() {
        let (_temporary, source, _destination) = roots();
        assert_eq!(
            inspect_skill_discovery(&source, &source, "docs").unwrap(),
            SkillDiscoveryState::Selected
        );
    }

    #[test]
    fn gemini_disabled_names_are_projected_without_rewriting_other_settings() {
        let original = b"{\n  \"theme\" : \"dark\",\n  \"skills\": {\"enabled\":true,\"disabled\":[\"old\"]}\n}\n";
        let disabled = project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "docs",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(disabled.contains("\"theme\" : \"dark\""));
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::GeminiSettings,
                Some(disabled.as_bytes()),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Disabled
        );

        let enabled = project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(disabled.as_bytes()),
            "docs",
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::GeminiSettings,
                Some(enabled.as_bytes()),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Enabled
        );
        assert!(enabled.contains("\"old\""));
    }

    #[test]
    fn gemini_disabled_names_follow_native_case_insensitive_matching() {
        let original = br#"{"skills":{"disabled":["docs","DOCS","other"]}}"#;
        assert_eq!(
            inspect_skill_config_state(SkillConfigTarget::GeminiSettings, Some(original), "Docs")
                .unwrap(),
            SkillConfigState::Disabled
        );
        assert!(project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "Docs",
            false,
        )
        .unwrap()
        .is_none());

        let enabled = project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "Docs",
            true,
        )
        .unwrap()
        .unwrap();
        let parsed: Value = serde_json::from_str(&enabled).unwrap();
        assert_eq!(parsed["skills"]["disabled"], serde_json::json!(["other"]));
    }

    #[test]
    fn gemini_disabled_names_preserve_json_comments() {
        let original = br#"{
          // keep this user note
          theme: 'dark',
          skills: {
            // keep this paths note
            paths: ['~/team'],
            disabled: ['old'],
          },
        }"#;
        let disabled = project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "docs",
            false,
        )
        .unwrap()
        .unwrap();

        assert!(disabled.contains("// keep this user note"));
        assert!(disabled.contains("// keep this paths note"));
        let parsed: Value = json5::from_str(&disabled).unwrap();
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["skills"]["paths"], serde_json::json!(["~/team"]));
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::GeminiSettings,
                Some(disabled.as_bytes()),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Disabled
        );
    }

    #[test]
    fn gemini_disabled_list_comments_are_read_only() {
        let original = br#"{
          skills: {
            disabled: ['old', // policy note
            ],
          },
        }"#;

        assert!(matches!(
            inspect_skill_config_state(SkillConfigTarget::GeminiSettings, Some(original), "docs"),
            Err(SkillConfigError::InvalidConfig { .. })
        ));
        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::GeminiSettings,
                Some(original),
                "docs",
                false
            ),
            Err(SkillConfigError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn gemini_skill_control_rejects_oversized_documents_before_parsing() {
        let oversized = vec![b' '; MAX_OPERATION_CONTENT_BYTES + 1];

        assert!(matches!(
            inspect_skill_config_state(
                SkillConfigTarget::GeminiSettings,
                Some(&oversized),
                "docs"
            ),
            Err(SkillConfigError::InvalidConfig { message, .. })
                if message == "document is too large"
        ));
    }

    #[test]
    fn gemini_global_disable_is_effective_and_cannot_be_overridden() {
        let original = br#"{"skills":{"enabled":false,"disabled":[]}}"#;
        assert_eq!(
            inspect_skill_config_state(SkillConfigTarget::GeminiSettings, Some(original), "docs")
                .unwrap(),
            SkillConfigState::GloballyDisabled
        );
        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::GeminiSettings,
                Some(original),
                "docs",
                true
            ),
            Err(SkillConfigError::GloballyDisabled {
                target: SkillConfigTarget::GeminiSettings
            })
        ));
        let disabled = project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "docs",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(disabled.contains("\"docs\""));
        let mut globally_enabled = serde_json::from_str::<serde_json::Value>(&disabled).unwrap();
        globally_enabled["skills"]["enabled"] = serde_json::Value::Bool(true);
        let globally_enabled = serde_json::to_vec(&globally_enabled).unwrap();
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::GeminiSettings,
                Some(&globally_enabled),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Disabled
        );
    }

    #[test]
    fn grok_disabled_names_preserve_unrelated_toml() {
        let original =
            b"model = \"grok-code\"\n\n[skills]\npaths = [\"~/team\"]\ndisabled = [\"old\"]\n";
        let disabled = project_skill_config_enabled(
            SkillConfigTarget::GrokConfig,
            Some(original),
            "docs",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(disabled.contains("model = \"grok-code\""));
        assert!(disabled.contains("paths = [\"~/team\"]"));
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::GrokConfig,
                Some(disabled.as_bytes()),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Disabled
        );
        assert!(project_skill_config_enabled(
            SkillConfigTarget::GrokConfig,
            Some(disabled.as_bytes()),
            "docs",
            false
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn grok_disabled_list_comments_are_read_only() {
        let original = b"[skills]\ndisabled = [\n  # team policy\n  \"Docs\",\n  \"Other\",\n]\n";

        assert!(matches!(
            inspect_skill_config_state(SkillConfigTarget::GrokConfig, Some(original), "Docs"),
            Err(SkillConfigError::InvalidConfig { .. })
        ));
        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::GrokConfig,
                Some(original),
                "Docs",
                true
            ),
            Err(SkillConfigError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn hermes_disabled_names_preserve_unrelated_yaml_and_platform_controls() {
        let original = b"# keep this comment\nmodel:\n  default: local\nskills:\n  config:\n    token: keep\n  disabled: [old]\n  platform_disabled:\n    telegram: [mobile]\n";
        let disabled = project_skill_config_enabled(
            SkillConfigTarget::HermesConfig,
            Some(original),
            "docs",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(disabled.contains("# keep this comment"));
        let parsed = serde_yaml::from_str::<serde_yaml::Value>(&disabled).unwrap();
        assert_eq!(parsed["model"]["default"], "local");
        assert_eq!(parsed["skills"]["config"]["token"], "keep");
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::HermesConfig,
                Some(disabled.as_bytes()),
                "docs"
            )
            .unwrap(),
            SkillConfigState::Disabled
        );
        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::HermesConfig,
                Some(disabled.as_bytes()),
                "mobile"
            )
            .unwrap(),
            SkillConfigState::ExternallyDisabled
        );
        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::HermesConfig,
                Some(disabled.as_bytes()),
                "mobile",
                true
            ),
            Err(SkillConfigError::ExternallyDisabled {
                target: SkillConfigTarget::HermesConfig
            })
        ));
    }

    #[test]
    fn hermes_required_skill_is_enabled_and_read_only() {
        let original = b"skills:\n  disabled: [hermes-agent, docs]\n";

        assert_eq!(
            inspect_skill_config_state(
                SkillConfigTarget::HermesConfig,
                Some(original),
                "hermes-agent"
            )
            .unwrap(),
            SkillConfigState::Required
        );
        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::HermesConfig,
                Some(original),
                "hermes-agent",
                false
            ),
            Err(SkillConfigError::RequiredSkill {
                target: SkillConfigTarget::HermesConfig,
                ..
            })
        ));
        let normalized = project_skill_config_enabled(
            SkillConfigTarget::HermesConfig,
            Some(original),
            "hermes-agent",
            true,
        )
        .unwrap()
        .unwrap();
        assert!(!normalized.contains("hermes-agent"));
        assert!(normalized.contains("docs"));
    }

    #[test]
    fn hermes_skill_comments_are_never_silently_rewritten() {
        let original = b"model: local\nskills:\n  # keep this note\n  disabled: [old]\n";

        assert!(matches!(
            project_skill_config_enabled(
                SkillConfigTarget::HermesConfig,
                Some(original),
                "docs",
                false
            ),
            Err(SkillConfigError::InvalidConfig { .. })
        ));
        assert!(!crate::yaml_patch::top_level_section_has_comments(
            "skills:\n  label: 'value # kept'\n",
            "skills"
        ));
    }

    #[test]
    fn hermes_skill_yaml_references_are_read_only() {
        for original in [
            b"defaults: &defaults\n  disabled: [docs]\nskills:\n  <<: *defaults\n".as_slice(),
            b"defaults: &defaults\n  token: keep\nskills:\n  config: *defaults\n  disabled: []\n"
                .as_slice(),
        ] {
            assert!(matches!(
                inspect_skill_config_state(SkillConfigTarget::HermesConfig, Some(original), "docs"),
                Err(SkillConfigError::InvalidConfig { .. })
            ));
            assert!(matches!(
                project_skill_config_enabled(
                    SkillConfigTarget::HermesConfig,
                    Some(original),
                    "docs",
                    false
                ),
                Err(SkillConfigError::InvalidConfig { .. })
            ));
        }
    }

    #[test]
    fn malformed_skill_control_sections_are_never_rewritten() {
        for (target, contents) in [
            (
                SkillConfigTarget::GeminiSettings,
                b"{\"skills\":[]}".as_slice(),
            ),
            (
                SkillConfigTarget::GeminiSettings,
                b"{\"skills\":{\"enabled\":\"yes\"}}".as_slice(),
            ),
            (SkillConfigTarget::GrokConfig, b"skills = []".as_slice()),
            (
                SkillConfigTarget::HermesConfig,
                b"skills:\n  platform_disabled: []\n".as_slice(),
            ),
        ] {
            assert!(matches!(
                project_skill_config_enabled(target, Some(contents), "docs", false),
                Err(SkillConfigError::InvalidConfig { .. })
            ));
        }
    }

    #[test]
    fn directory_budget_is_enforced_while_entries_are_collected() {
        let temporary = tempdir().unwrap();
        fs::write(temporary.path().join("a"), "a").unwrap();
        fs::write(temporary.path().join("b"), "b").unwrap();
        let mut budget = TreeBudget {
            entries: MAX_SKILL_TREE_ENTRIES - 1,
            bytes: 0,
        };

        assert!(matches!(
            read_directory_entries(temporary.path(), &mut budget, false),
            Err(SkillConfigError::EntryLimit {
                limit: MAX_SKILL_TREE_ENTRIES
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn tree_digest_frames_file_contents_unambiguously() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        for root in [&source, &destination] {
            fs::write(root.join("SKILL.md"), "manifest").unwrap();
        }
        fs::write(destination.join("a"), b"x").unwrap();
        fs::write(destination.join("b"), b"y").unwrap();

        let mode = fs::metadata(destination.join("b"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let mut ambiguous = b"x\0f\0b\0".to_vec();
        ambiguous.extend_from_slice(&mode.to_le_bytes());
        ambiguous.extend_from_slice(b"y");
        fs::write(source.join("a"), ambiguous).unwrap();
        fs::set_permissions(
            source.join("a"),
            fs::metadata(destination.join("a")).unwrap().permissions(),
        )
        .unwrap();

        assert_ne!(
            tree_digest(&source).unwrap(),
            tree_digest(&destination).unwrap()
        );
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
    fn managed_copy_ownership_survives_a_shared_source_update() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        fs::write(source.join("docs/SKILL.md"), "# Updated\n").unwrap();

        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );
        apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert!(!destination.join("docs").exists());
    }

    #[test]
    fn identical_unmanaged_copy_is_read_only_and_preserved() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        copy_tree(
            &source.join("docs"),
            &destination.join("docs"),
            &mut TreeBudget::default(),
        )
        .unwrap();

        assert!(matches!(
            inspect_skill_deployment(&source, &destination, "docs"),
            Err(SkillConfigError::Conflict { .. })
        ));
        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy),
            Err(SkillConfigError::Conflict { .. })
        ));
        assert!(destination.join("docs/SKILL.md").is_file());
    }

    #[test]
    fn host_evidence_can_manage_an_exact_legacy_copy() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        copy_tree(
            &source.join("docs"),
            &destination.join("docs"),
            &mut TreeBudget::default(),
        )
        .unwrap();

        assert_eq!(
            inspect_skill_deployment_with_policy(
                &source,
                &destination,
                "docs",
                SkillCopyPolicy::AllowMatching
            )
            .unwrap(),
            SkillDeploymentState::Copied
        );
        apply_skill_deployment_with_policy(
            &source,
            &destination,
            "docs",
            false,
            SkillSyncMethod::Copy,
            SkillCopyPolicy::AllowMatching,
        )
        .unwrap()
        .commit()
        .unwrap();
        assert!(!destination.join("docs").exists());
        assert!(source.join("docs/SKILL.md").exists());
    }

    #[test]
    fn interrupted_legacy_copy_disable_reuses_host_evidence() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        copy_tree(
            &source.join("docs"),
            &destination.join("docs"),
            &mut TreeBudget::default(),
        )
        .unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Disable)
                .unwrap();
        fs::rename(destination.join("docs"), temporary_root.join("deployment")).unwrap();

        apply_skill_deployment_with_policy(
            &source,
            &destination,
            "docs",
            false,
            SkillSyncMethod::Copy,
            SkillCopyPolicy::AllowMatching,
        )
        .unwrap()
        .commit()
        .unwrap();

        assert!(!destination.join("docs").exists());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn incomplete_markers_do_not_block_other_skill_operations() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let malformed = destination.join(format!("{TEMP_DIRECTORY_PREFIX}malformed"));
        fs::create_dir(&malformed).unwrap();
        fs::write(malformed.join(TEMP_MARKER_FILE), b"{").unwrap();
        let pending = destination.join(format!("{TEMP_DIRECTORY_PREFIX}pending"));
        fs::create_dir(&pending).unwrap();
        fs::write(pending.join(TEMP_MARKER_STAGING_FILE), b"{").unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(malformed.exists());
        assert!(pending.exists());
        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );
    }

    #[test]
    fn interrupted_copy_is_completed_before_retry() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        assert!(temporary_root.join(TEMP_MARKER_FILE).is_file());
        assert!(!temporary_root.join(TEMP_MARKER_STAGING_FILE).exists());
        copy_tree(
            &source.join("docs"),
            &temporary_root.join("deployment"),
            &mut TreeBudget::default(),
        )
        .unwrap();
        let digest = tree_digest(&source.join("docs")).unwrap();
        write_managed_copy_marker(&temporary_root.join("deployment"), "docs", &digest).unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );
        assert!(!temporary_root.exists());
    }

    #[test]
    fn interrupted_copy_is_rebuilt_when_its_source_changes() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        copy_tree(
            &source.join("docs"),
            &temporary_root.join("deployment"),
            &mut TreeBudget::default(),
        )
        .unwrap();
        let digest = tree_digest(&source.join("docs")).unwrap();
        write_managed_copy_marker(&temporary_root.join("deployment"), "docs", &digest).unwrap();
        fs::write(source.join("docs/SKILL.md"), "# Current Docs\n").unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("docs/SKILL.md")).unwrap(),
            "# Current Docs\n"
        );
        assert!(!temporary_root.exists());
    }

    #[test]
    fn empty_tagged_operation_is_removed_and_retried() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root = destination.join(format!(
            "{TEMP_DIRECTORY_PREFIX}{}.crash",
            operation_directory_tag("docs")
        ));
        fs::create_dir(&temporary_root).unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(destination.join("docs/SKILL.md").is_file());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn interrupted_enable_discards_owned_partial_stage_and_retries() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        fs::create_dir(temporary_root.join("deployment")).unwrap();
        fs::write(
            temporary_root.join("deployment/SKILL.md"),
            "changed while interrupted",
        )
        .unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("docs/SKILL.md")).unwrap(),
            "# Docs\n"
        );
        assert!(!temporary_root.exists());
    }

    #[test]
    fn interrupted_enable_reuses_host_evidence_for_a_legacy_copy() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        copy_tree(
            &source.join("docs"),
            &destination.join("docs"),
            &mut TreeBudget::default(),
        )
        .unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();

        apply_skill_deployment_with_policy(
            &source,
            &destination,
            "docs",
            true,
            SkillSyncMethod::Copy,
            SkillCopyPolicy::AllowMatching,
        )
        .unwrap()
        .commit()
        .unwrap();

        assert!(destination.join("docs/SKILL.md").is_file());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn staged_operation_marker_is_finalized_during_recovery() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        fs::rename(
            temporary_root.join(TEMP_MARKER_FILE),
            temporary_root.join(TEMP_MARKER_STAGING_FILE),
        )
        .unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(destination.join("docs/SKILL.md").is_file());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn unrelated_future_marker_does_not_block_another_skill() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "other", InterruptedOperation::Enable)
                .unwrap();
        fs::write(
            temporary_root.join(TEMP_MARKER_FILE),
            serde_json::to_vec(&InterruptedOperationMarker {
                version: TEMP_MARKER_VERSION + 1,
                operation: InterruptedOperation::Enable,
                directory: "other".to_owned(),
            })
            .unwrap(),
        )
        .unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(temporary_root.exists());
        assert_eq!(
            inspect_skill_deployment(&source, &destination, "docs").unwrap(),
            SkillDeploymentState::Copied
        );
    }

    #[test]
    fn interrupted_disable_is_completed_before_retry() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Disable)
                .unwrap();
        fs::rename(destination.join("docs"), temporary_root.join("deployment")).unwrap();

        apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(!destination.join("docs").exists());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn interrupted_disable_is_rolled_back_when_retry_enables() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Disable)
                .unwrap();
        fs::rename(destination.join("docs"), temporary_root.join("deployment")).unwrap();

        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(destination.join("docs/SKILL.md").is_file());
        assert!(!temporary_root.exists());
    }

    #[test]
    fn interrupted_disable_preserves_a_changed_partial_backup() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let receipt =
            apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
                .unwrap();
        let temporary_root = match &receipt.change {
            DeploymentChange::Removed { temporary_root, .. } => temporary_root.clone(),
            _ => panic!("disable must retain a temporary backup"),
        };
        fs::remove_file(temporary_root.join("deployment/SKILL.md")).unwrap();
        drop(receipt);

        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy),
            Err(SkillConfigError::Recovery { .. })
        ));

        assert!(!destination.join("docs").exists());
        assert!(temporary_root.exists());
    }

    #[test]
    fn invalid_tagged_marker_blocks_only_its_skill() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(source.join("other")).unwrap();
        fs::write(source.join("other/SKILL.md"), "# Other\n").unwrap();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        fs::create_dir(temporary_root.join("deployment")).unwrap();
        fs::write(temporary_root.join(TEMP_MARKER_FILE), b"{").unwrap();

        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy),
            Err(SkillConfigError::Recovery { .. })
        ));
        apply_skill_deployment(&source, &destination, "other", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        assert!(destination.join("other/SKILL.md").is_file());
    }

    #[test]
    fn unidentified_temporary_content_is_never_deleted() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        fs::write(temporary_root.join(TEMP_MARKER_FILE), b"{").unwrap();
        fs::write(temporary_root.join("keep.txt"), b"unknown").unwrap();

        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy),
            Err(SkillConfigError::Recovery { .. })
        ));
        assert_eq!(
            fs::read(temporary_root.join("keep.txt")).unwrap(),
            b"unknown"
        );
    }

    #[test]
    fn unidentified_marker_only_operation_is_never_deleted() {
        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Enable).unwrap();
        fs::write(temporary_root.join(TEMP_MARKER_FILE), b"{").unwrap();

        assert!(matches!(
            apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy),
            Err(SkillConfigError::Recovery { .. })
        ));
        assert_eq!(
            fs::read(temporary_root.join(TEMP_MARKER_FILE)).unwrap(),
            b"{"
        );
    }

    #[test]
    fn marker_only_disable_is_finished_before_retry() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Disable)
                .unwrap();

        apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();

        assert!(!destination.join("docs").exists());
        assert!(!temporary_root.exists());
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
    fn rollback_restores_the_object_that_was_moved_even_if_it_changed() {
        let (_temporary, source, destination) = roots();
        apply_skill_deployment(&source, &destination, "docs", true, SkillSyncMethod::Copy)
            .unwrap()
            .commit()
            .unwrap();
        let receipt =
            apply_skill_deployment(&source, &destination, "docs", false, SkillSyncMethod::Copy)
                .unwrap();
        let backup = match &receipt.change {
            DeploymentChange::Removed { backup, .. } => backup,
            _ => panic!("disable must move the deployment"),
        };
        fs::write(backup.join("SKILL.md"), "changed while pending").unwrap();

        receipt.rollback().unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("docs/SKILL.md")).unwrap(),
            "changed while pending"
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

    #[test]
    fn missing_parent_segments_cannot_hide_overlapping_roots() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        let aliased = temporary.path().join("missing").join("..").join("source");

        assert!(matches!(
            ensure_distinct_roots(&source, &aliased),
            Err(SkillConfigError::OverlappingRoots { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn enable_rechecks_root_identity_after_the_destination_parent_appears() {
        use std::os::unix::fs::symlink;

        let (_temporary, source, destination) = roots();
        let paths = deployment_paths(&source, &destination, "docs").unwrap();
        symlink(&source, &destination).unwrap();

        assert!(matches!(
            enable_deployment(
                paths,
                "docs",
                SkillSyncMethod::Copy,
                SkillCopyPolicy::ManagedOnly
            ),
            Err(SkillConfigError::OverlappingRoots { .. })
        ));
        assert_eq!(
            fs::read_to_string(source.join("docs/SKILL.md")).unwrap(),
            "# Docs\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn disable_rechecks_root_identity_before_moving_a_matching_copy() {
        use std::os::unix::fs::symlink;

        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        copy_tree(
            &source.join("docs"),
            &destination.join("docs"),
            &mut TreeBudget::default(),
        )
        .unwrap();
        let paths = deployment_paths(&source, &destination, "docs").unwrap();
        let (_, expectation) = inspect_paths_with_policy(
            &paths.source,
            &paths.destination,
            "docs",
            SkillCopyPolicy::AllowMatching,
        )
        .unwrap();
        fs::remove_dir_all(&destination).unwrap();
        symlink(&source, &destination).unwrap();

        assert!(matches!(
            disable_deployment(paths, "docs", expectation),
            Err(SkillConfigError::OverlappingRoots { .. })
        ));
        assert_eq!(
            fs::read_to_string(source.join("docs/SKILL.md")).unwrap(),
            "# Docs\n"
        );
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

    #[cfg(unix)]
    #[test]
    fn interrupted_disable_recovers_a_relative_link_from_its_original_parent() {
        use std::os::unix::fs::symlink;

        let (_temporary, source, destination) = roots();
        fs::create_dir_all(&destination).unwrap();
        symlink("../source/docs", destination.join("docs")).unwrap();
        let temporary_root =
            create_temporary_directory(&destination, "docs", InterruptedOperation::Disable)
                .unwrap();
        fs::rename(destination.join("docs"), temporary_root.join("deployment")).unwrap();

        apply_skill_deployment(
            &source,
            &destination,
            "docs",
            false,
            SkillSyncMethod::Symlink,
        )
        .unwrap()
        .commit()
        .unwrap();

        assert!(!destination.join("docs").exists());
        assert!(!temporary_root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn path_identity_resolves_existing_directory_aliases() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let root = temporary.path().join("root");
        let alias = temporary.path().join("alias");
        fs::create_dir(&root).unwrap();
        symlink(&root, &alias).unwrap();

        assert_eq!(
            skill_path_identity(&root.join("skills")).unwrap(),
            skill_path_identity(&alias.join("skills")).unwrap()
        );
    }
}
