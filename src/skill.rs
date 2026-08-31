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

/// How a host may control a Skill that an application discovers directly from
/// `~/.agents/skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnifiedSkillControl {
    /// Core cannot safely determine or change the application's effective state.
    ReadOnly,
    /// The application uses a name-based disabled list in its native settings.
    DisabledNameList(SkillConfigTarget),
}

/// Native document containing a supported per-Skill disabled list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillConfigTarget {
    GeminiSettings,
    GrokConfig,
}

/// Effective state reported by a supported native per-Skill control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillConfigState {
    Enabled,
    Disabled,
    GloballyDisabled,
}

impl SkillConfigTarget {
    /// Returns the shared logical document edited by this control.
    pub const fn logical_target(self) -> LogicalTarget {
        match self {
            Self::GeminiSettings => LogicalTarget::GeminiSettings,
            Self::GrokConfig => LogicalTarget::GrokConfig,
        }
    }
}

/// Product-neutral Skill behavior declared by an application descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillAppContract {
    catalog_column: Option<&'static str>,
    unified_control: Option<UnifiedSkillControl>,
}

impl SkillAppContract {
    /// Declares an application's stable selection column in the shared catalog.
    pub const fn with_catalog_column(column: &'static str) -> Self {
        Self {
            catalog_column: Some(column),
            unified_control: None,
        }
    }

    /// Declares an application that has no column in the shared catalog.
    pub const fn without_catalog() -> Self {
        Self {
            catalog_column: None,
            unified_control: None,
        }
    }

    /// Declares that the application discovers `~/.agents/skills` directly.
    pub const fn with_unified_store_discovery(mut self, control: UnifiedSkillControl) -> Self {
        self.unified_control = Some(control);
        self
    }

    /// Returns the shared catalog column used to persist the requested selection.
    pub const fn catalog_column(self) -> Option<&'static str> {
        self.catalog_column
    }

    /// Returns direct unified-store discovery and its supported control method.
    pub const fn unified_control(self) -> Option<UnifiedSkillControl> {
        self.unified_control
    }
}

pub const CLAUDE_SKILLS: SkillAppContract = SkillAppContract::with_catalog_column("enabled_claude");
pub const CODEX_SKILLS: SkillAppContract = SkillAppContract::with_catalog_column("enabled_codex")
    .with_unified_store_discovery(UnifiedSkillControl::ReadOnly);
pub const GEMINI_SKILLS: SkillAppContract = SkillAppContract::with_catalog_column("enabled_gemini")
    .with_unified_store_discovery(UnifiedSkillControl::DisabledNameList(
        SkillConfigTarget::GeminiSettings,
    ));
pub const GROKBUILD_SKILLS: SkillAppContract =
    SkillAppContract::with_catalog_column("enabled_grokbuild").with_unified_store_discovery(
        UnifiedSkillControl::DisabledNameList(SkillConfigTarget::GrokConfig),
    );
pub const OPENCODE_SKILLS: SkillAppContract =
    SkillAppContract::with_catalog_column("enabled_opencode")
        .with_unified_store_discovery(UnifiedSkillControl::ReadOnly);
pub const HERMES_SKILLS: SkillAppContract = SkillAppContract::with_catalog_column("enabled_hermes");
pub const PI_SKILLS: SkillAppContract =
    SkillAppContract::without_catalog().with_unified_store_discovery(UnifiedSkillControl::ReadOnly);

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
    match inspect_skill_deployment(source_root, discovery_root, directory) {
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
    let disabled = match target {
        SkillConfigTarget::GeminiSettings => {
            let root = parse_skill_json(target, contents)?;
            if !json_skills_enabled(target, &root)? {
                return Ok(SkillConfigState::GloballyDisabled);
            }
            json_disabled_names(target, &root)?
        }
        SkillConfigTarget::GrokConfig => {
            toml_disabled_names(target, &parse_skill_toml(target, contents)?)?
        }
    };
    Ok(if disabled.iter().any(|entry| entry == name) {
        SkillConfigState::Disabled
    } else {
        SkillConfigState::Enabled
    })
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
    match target {
        SkillConfigTarget::GeminiSettings => {
            project_json_skill_enabled(target, contents, name, enabled)
        }
        SkillConfigTarget::GrokConfig => {
            project_toml_skill_enabled(target, contents, name, enabled)
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
    let mut root = parse_skill_json(target, contents)?;
    let skills_enabled = json_skills_enabled(target, &root)?;
    if enabled && !skills_enabled {
        return Err(SkillConfigError::GloballyDisabled { target });
    }
    let disabled = json_disabled_names(target, &root)?;
    let currently_enabled = skills_enabled && !disabled.iter().any(|entry| entry == name);
    if currently_enabled == enabled {
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
        .filter(|entry| !enabled || entry != name)
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
        crate::json_patch::replace_top_level_value(
            original,
            "skills",
            root.get("skills").expect("projected JSON contains skills"),
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
    let root = serde_json::from_slice::<Value>(contents)
        .map_err(|_| invalid_skill_config(target, "document is not strict JSON"))?;
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
    disabled
        .iter()
        .map(|entry| {
            entry.as_str().map(str::to_owned).ok_or_else(|| {
                invalid_skill_config(target, "'skills.disabled' must contain strings")
            })
        })
        .collect()
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
    let paths = deployment_paths(source_root, destination_root, directory)?;
    recover_interrupted_deployment(&paths, directory)?;
    let (state, expectation) = inspect_paths(&paths.source, &paths.destination)?;
    if enabled {
        return match state {
            SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
                Ok(observed_receipt(paths.destination, expectation))
            }
            SkillDeploymentState::Missing => enable_deployment(paths, directory, sync_method),
        };
    }

    match state {
        SkillDeploymentState::Missing => Ok(observed_receipt(paths.destination, expectation)),
        SkillDeploymentState::Linked | SkillDeploymentState::Copied => {
            disable_deployment(paths.destination, directory, expectation)
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
    directory: &str,
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

    create_copy(paths, directory)
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
    destination: PathBuf,
    directory: &str,
    expectation: DeploymentExpectation,
) -> Result<SkillDeploymentReceipt, SkillConfigError> {
    let parent = destination
        .parent()
        .expect("a deployment has a destination root");
    let temporary_root =
        create_temporary_directory(parent, directory, InterruptedOperation::Disable)?;
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
    hasher.update(b"cc-switch-skill-tree-v1\0");
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
    let entries = read_directory_entries(directory, budget)?;

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
    let entries = read_directory_entries(source, budget)?;
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
        } else {
            return Err(SkillConfigError::UnsupportedEntry { path: source_path });
        }
    }
    Ok(())
}

fn read_directory_entries(
    directory: &Path,
    budget: &mut TreeBudget,
) -> Result<Vec<DirEntry>, SkillConfigError> {
    let reader =
        fs::read_dir(directory).map_err(|source| SkillConfigError::io(directory, source))?;
    let mut entries = Vec::new();
    for entry in reader {
        budget.add_entry()?;
        entries.push(entry.map_err(|source| SkillConfigError::io(directory, source))?);
    }
    entries.sort_by_key(DirEntry::file_name);
    Ok(entries)
}

fn recover_interrupted_deployment(
    paths: &DeploymentPaths,
    directory: &str,
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
        let entry = entry.map_err(|source| SkillConfigError::io(parent, source))?;
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
        let Some(marker) = read_operation_marker(&temporary_root)? else {
            continue;
        };
        if marker.directory != directory {
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
            InterruptedOperation::Enable => recover_interrupted_enable(paths, &temporary_root)?,
            InterruptedOperation::Disable => recover_interrupted_disable(paths, &temporary_root)?,
        }
    }
    Ok(())
}

fn read_operation_marker(
    temporary_root: &Path,
) -> Result<Option<InterruptedOperationMarker>, SkillConfigError> {
    let path = temporary_root.join(TEMP_MARKER_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SkillConfigError::io(path, source)),
    };
    if metadata.len() > MAX_TEMP_MARKER_BYTES {
        return Ok(None);
    }
    let contents = fs::read(&path).map_err(|source| SkillConfigError::io(&path, source))?;
    Ok(serde_json::from_slice(&contents).ok())
}

fn recover_interrupted_enable(
    paths: &DeploymentPaths,
    temporary_root: &Path,
) -> Result<(), SkillConfigError> {
    let (destination_state, _) = inspect_paths(&paths.source, &paths.destination)?;
    if destination_state == SkillDeploymentState::Missing {
        let staged = temporary_root.join("deployment");
        match inspect_paths(&paths.source, &staged) {
            Ok((SkillDeploymentState::Linked | SkillDeploymentState::Copied, _)) => {
                fs::rename(&staged, &paths.destination)
                    .map_err(|source| SkillConfigError::io(&paths.destination, source))?;
            }
            Ok((SkillDeploymentState::Missing, _)) | Err(SkillConfigError::Conflict { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    remove_directory(temporary_root)
}

fn recover_interrupted_disable(
    paths: &DeploymentPaths,
    temporary_root: &Path,
) -> Result<(), SkillConfigError> {
    let backup = temporary_root.join("deployment");
    let (destination_state, _) = inspect_paths(&paths.source, &paths.destination)?;
    let (backup_state, _) = inspect_paths(&paths.source, &backup)?;
    match (destination_state, backup_state) {
        (SkillDeploymentState::Missing, _)
        | (
            SkillDeploymentState::Linked | SkillDeploymentState::Copied,
            SkillDeploymentState::Missing,
        ) => {
            if destination_state != SkillDeploymentState::Missing {
                remove_deployment(&paths.destination)?;
            }
            remove_directory(temporary_root)
        }
        _ => Err(SkillConfigError::Recovery {
            message: format!(
                "interrupted Skill disable has both destination and backup at {:?}",
                paths.destination
            ),
        }),
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
    for _ in 0..16 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "{TEMP_DIRECTORY_PREFIX}{}.{timestamp}.{counter}",
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
    if let Err(source) = fs::rename(&staging, &path) {
        let _ = fs::remove_file(&staging);
        return Err(SkillConfigError::io(&path, source));
    }
    sync_directory(temporary_root)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), SkillConfigError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| SkillConfigError::io(path, source))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), SkillConfigError> {
    Ok(())
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
    if metadata.file_type().is_symlink() {
        remove_directory_symlink(path)
    } else if metadata.file_type().is_file() {
        fs::remove_file(path).map_err(|source| SkillConfigError::io(path, source))
    } else if metadata.file_type().is_dir() {
        remove_directory(path)
    } else {
        Err(SkillConfigError::UnsupportedEntry {
            path: path.to_owned(),
        })
    }
}

#[cfg(unix)]
fn remove_directory_symlink(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_file(path).map_err(|source| SkillConfigError::io(path, source))
}

#[cfg(windows)]
fn remove_directory_symlink(path: &Path) -> Result<(), SkillConfigError> {
    fs::remove_dir(path).map_err(|source| SkillConfigError::io(path, source))
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
        assert!(project_skill_config_enabled(
            SkillConfigTarget::GeminiSettings,
            Some(original),
            "docs",
            false
        )
        .unwrap()
        .is_none());
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
            read_directory_entries(temporary.path(), &mut budget),
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
