//! Pure MCP configuration adapters shared by CC Switch products.
//!
//! Hosts own paths, locking, persistence, and rollback. This module validates
//! the unified server shape and changes only the MCP section of live config.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike};

use crate::{AppType, LogicalTarget, MAX_OPERATION_CONTENT_BYTES};

mod json_patch;

const MAX_MCP_ID_BYTES: usize = 128;
const MANAGED_SERVER_FIELDS: &[&str] = &["type", "command", "args", "env", "cwd", "url", "headers"];
const MCP_METADATA_FIELDS: &[&str] = &[
    "enabled",
    "source",
    "id",
    "name",
    "description",
    "tags",
    "homepage",
    "docs",
    "server",
];
const GEMINI_TIMEOUT_FIELDS: &[&str] = &[
    "timeout",
    "startup_timeout_sec",
    "startup_timeout_ms",
    "tool_timeout_sec",
    "tool_timeout_ms",
];
const NATIVE_ALIAS_FIELDS: &[(&str, &str)] = &[
    ("environment", "env"),
    ("http_headers", "headers"),
    ("httpUrl", "url"),
];

/// One application-owned MCP configuration document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpConfigTarget {
    Claude,
    Codex,
    Gemini,
    GrokBuild,
    OpenCode,
    Hermes,
}

/// Field selection when decoding a native MCP entry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpEntryDecodePolicy {
    /// Keep the existing structural codec, including native extensions.
    #[default]
    Preserve,
    /// Extract tolerant Codex transport fields only. String arrays/maps discard
    /// non-string members and empty results; extensions and enablement stay with
    /// the caller. Explicit invalid types are retained for caller validation.
    /// Other native formats currently reject this policy.
    TransportFields,
}

impl McpConfigTarget {
    /// Decodes an entry with explicit field selection, without validating a
    /// connection, choosing catalog metadata, or reading a document.
    ///
    /// [`McpEntryDecodePolicy::TransportFields`] currently supports Codex. It
    /// infers HTTP only from a nonblank string URL when `type` is absent, and
    /// prefers an object-valued `http_headers` over legacy `headers`, even when
    /// that object is empty. The default [`Self::decode_server`] is unchanged.
    pub fn decode_server_with_policy(
        self,
        entry: &Value,
        policy: McpEntryDecodePolicy,
    ) -> Result<Value, McpConfigError> {
        match policy {
            McpEntryDecodePolicy::Preserve => self.decode_server(entry),
            McpEntryDecodePolicy::TransportFields if self == Self::Codex => {
                Ok(decode_codex_transport_fields(server_object(entry)?))
            }
            McpEntryDecodePolicy::TransportFields => {
                Err(McpConfigError::UnsupportedEntryPolicy { target: self })
            }
        }
    }

    /// Converts one native entry to the shared transport field names.
    ///
    /// This is a structural codec, not validation or catalog import. Extension
    /// fields may remain in the result; hosts choose which fields to import and
    /// read native enablement separately. Use [`validate_mcp_server`] to check a
    /// connection, or [`import_mcp_servers`] for validated document import.
    pub fn decode_server(self, entry: &Value) -> Result<Value, McpConfigError> {
        match self {
            Self::Claude => from_json_flavor(JsonFlavor::Claude, entry).map(|entry| entry.0),
            Self::Gemini => from_json_flavor(JsonFlavor::Gemini, entry).map(|entry| entry.0),
            Self::OpenCode => from_json_flavor(JsonFlavor::OpenCode, entry).map(|entry| entry.0),
            Self::Hermes => from_hermes(entry),
            Self::Codex | Self::GrokBuild => {
                let mut server = server_object(entry)?.clone();
                normalize_toml_server(&mut server);
                Ok(Value::Object(server))
            }
        }
    }

    /// Converts shared transport fields into a new native entry.
    ///
    /// This does not validate a connection or merge an existing live entry.
    /// The result uses JSON values; native serialization may reject values that
    /// its format cannot represent (for example a TOML extension containing null).
    /// Native enablement defaults to on where the format needs it. Hosts retain
    /// their field-selection and validation policies; use [`project_mcp_server`]
    /// for validated, loss-aware updates to an existing document.
    pub fn encode_server(self, server: &Value) -> Result<Value, McpConfigError> {
        match self {
            Self::Claude => to_json_flavor(JsonFlavor::Claude, server, None),
            Self::Gemini => to_json_flavor(JsonFlavor::Gemini, server, None),
            Self::OpenCode => to_json_flavor(JsonFlavor::OpenCode, server, None),
            Self::Hermes => to_hermes(server, None, true),
            Self::Codex | Self::GrokBuild => Ok(Value::Object(
                toml_server_fields(server_object(server)?, self == Self::GrokBuild)
                    .map(|(key, value, _)| (key.to_owned(), value.clone()))
                    .collect(),
            )),
        }
    }
}

/// Native document that stores one application's MCP configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum McpConfigResource {
    /// MCP shares an existing native configuration target.
    LogicalTarget(LogicalTarget),
    /// MCP uses a host-resolved resource outside the app's native targets.
    HostDefined,
}

/// MCP behavior that a host must honor for one application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpAppContract {
    catalog_column: McpCatalogColumn,
    target: McpConfigTarget,
    resource: McpConfigResource,
    preserves_disabled_entry: bool,
    supports_cwd: bool,
}

impl McpAppContract {
    /// Returns the shared-catalog column that stores this application's state.
    pub const fn catalog_column(self) -> McpCatalogColumn {
        self.catalog_column
    }

    /// Returns the application-owned MCP document target.
    pub const fn target(self) -> McpConfigTarget {
        self.target
    }

    /// Returns the native document that stores this application's MCP section.
    pub const fn resource(self) -> McpConfigResource {
        self.resource
    }

    /// Returns whether disabling keeps a native entry that can later be restored in place.
    pub const fn preserves_disabled_entry(self) -> bool {
        self.preserves_disabled_entry
    }

    /// Returns whether this application's native MCP format can express `cwd`.
    pub const fn supports_cwd(self) -> bool {
        self.supports_cwd
    }
}

/// A schema-backed `mcp_servers` selection column declared by Core.
///
/// Hosts can read its identifier but cannot construct arbitrary columns.
///
/// ```compile_fail
/// use cc_switch_core::McpCatalogColumn;
/// let _ = McpCatalogColumn("enabled_unknown");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpCatalogColumn(&'static str);

impl McpCatalogColumn {
    pub(crate) const fn new(column: &'static str) -> Self {
        Self(column)
    }

    /// Returns the fixed database identifier.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

pub(crate) const CLAUDE_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_claude"),
    target: McpConfigTarget::Claude,
    resource: McpConfigResource::HostDefined,
    preserves_disabled_entry: false,
    supports_cwd: true,
};
pub(crate) const CODEX_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_codex"),
    target: McpConfigTarget::Codex,
    resource: McpConfigResource::LogicalTarget(LogicalTarget::CodexConfig),
    preserves_disabled_entry: true,
    supports_cwd: true,
};
pub(crate) const GEMINI_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_gemini"),
    target: McpConfigTarget::Gemini,
    resource: McpConfigResource::LogicalTarget(LogicalTarget::GeminiSettings),
    preserves_disabled_entry: false,
    supports_cwd: true,
};
pub(crate) const GROKBUILD_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_grokbuild"),
    target: McpConfigTarget::GrokBuild,
    resource: McpConfigResource::LogicalTarget(LogicalTarget::GrokConfig),
    preserves_disabled_entry: true,
    supports_cwd: true,
};
pub(crate) const OPENCODE_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_opencode"),
    target: McpConfigTarget::OpenCode,
    resource: McpConfigResource::LogicalTarget(LogicalTarget::OpenCodeConfig),
    preserves_disabled_entry: true,
    supports_cwd: false,
};
pub(crate) const HERMES_MCP: McpAppContract = McpAppContract {
    catalog_column: McpCatalogColumn::new("enabled_hermes"),
    target: McpConfigTarget::Hermes,
    resource: McpConfigResource::LogicalTarget(LogicalTarget::HermesConfig),
    preserves_disabled_entry: true,
    supports_cwd: false,
};

/// Opaque, application-owned state used to restore an entry that has no native disabled form.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpNativeSnapshot {
    target: McpConfigTarget,
    #[serde(rename = "entry", with = "raw_snapshot_entry")]
    entry_json: String,
}

mod raw_snapshot_entry {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S>(entry: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RawValue::from_string(entry.to_owned())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        Box::<RawValue>::deserialize(deserializer).map(|entry| entry.get().to_owned())
    }
}

impl fmt::Debug for McpNativeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpNativeSnapshot")
            .field("target", &self.target)
            .field("entry", &"<redacted>")
            .finish()
    }
}

/// One valid server discovered in an application's live MCP document.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpImport {
    pub id: String,
    pub server: Value,
    pub enabled: bool,
    #[serde(skip_serializing)]
    pub native_snapshot: Option<McpNativeSnapshot>,
}

/// Desired state for one server in an application-owned MCP document.
#[derive(Clone, Copy)]
pub enum McpServerProjection<'a> {
    /// Write the shared connection fields and enable the native entry.
    Enable(&'a Value),
    /// Enable an entry, restoring target-owned fields when the live entry was removed on disable.
    Restore {
        server: &'a Value,
        snapshot: &'a McpNativeSnapshot,
    },
    /// Update an existing native entry and preserve native disabled state when supported.
    /// Applications without a native disabled state remove the live entry.
    Disable(&'a Value),
    /// Remove the native entry completely.
    Remove,
}

impl fmt::Debug for McpServerProjection<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enable(_) => formatter.write_str("Enable(<redacted>)"),
            Self::Restore { .. } => formatter.write_str("Restore(<redacted>)"),
            Self::Disable(_) => formatter.write_str("Disable(<redacted>)"),
            Self::Remove => formatter.write_str("Remove"),
        }
    }
}

impl fmt::Debug for McpImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpImport")
            .field("id", &self.id)
            .field("server", &"<redacted>")
            .field("enabled", &self.enabled)
            .field(
                "native_snapshot",
                &self.native_snapshot.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// MCP document or unified-server validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum McpConfigError {
    #[error("application '{app_id}' does not support MCP")]
    UnsupportedApp { app_id: String },
    #[error("native MCP format '{target:?}' does not support this entry decoding policy")]
    UnsupportedEntryPolicy { target: McpConfigTarget },
    #[error("MCP server id is invalid: {0}")]
    InvalidId(String),
    #[error("MCP server definition is invalid: {0}")]
    InvalidServer(String),
    #[error("{app_id} MCP configuration is invalid: {message}")]
    InvalidDocument { app_id: String, message: String },
}

/// Returns the MCP behavior declared for an application.
pub fn mcp_app_contract(app: &AppType) -> Option<&'static McpAppContract> {
    crate::builtin_app_registry().for_app(app).mcp_contract()
}

/// Returns every shared `mcp_servers.enabled_*` column in registry order.
pub fn mcp_catalog_columns() -> impl Iterator<Item = McpCatalogColumn> + Clone {
    crate::builtin_app_registry()
        .descriptors()
        .filter_map(|descriptor| {
            descriptor
                .mcp_contract()
                .map(|contract| contract.catalog_column())
        })
}

/// Returns the live MCP target declared for an application.
pub fn mcp_config_target(app: &AppType) -> Option<McpConfigTarget> {
    mcp_app_contract(app).map(|contract| contract.target())
}

/// Validates the small, cross-product MCP server contract.
pub fn validate_mcp_server(id: &str, server: &Value) -> Result<(), McpConfigError> {
    validate_id(id)?;
    let object = server.as_object().ok_or_else(|| {
        McpConfigError::InvalidServer("the definition must be an object".to_owned())
    })?;
    for (native, canonical) in NATIVE_ALIAS_FIELDS {
        if object.contains_key(*native) {
            return Err(McpConfigError::InvalidServer(format!(
                "'{native}' is a native field; use canonical '{canonical}'"
            )));
        }
    }
    let transport = match object.get("type") {
        Some(Value::String(transport)) => transport.as_str(),
        Some(_) => {
            return Err(McpConfigError::InvalidServer(
                "'type' must be a string".to_owned(),
            ));
        }
        None => "stdio",
    };
    match transport {
        "stdio" => {
            required_string(object, "command", "stdio definitions require command")?;
            string_array(object, "args")?;
            string_map(object, "env")?;
            optional_string(object, "cwd")?;
            reject_fields(object, &["url", "headers"], "stdio")?;
        }
        "http" | "sse" => {
            required_string(object, "url", "remote definitions require url")?;
            string_map(object, "headers")?;
            reject_fields(object, &["command", "args", "env", "cwd"], transport)?;
        }
        other => {
            return Err(McpConfigError::InvalidServer(format!(
                "unsupported transport '{other}'"
            )))
        }
    }
    if serde_json::to_vec(server)
        .map_err(|error| McpConfigError::InvalidServer(error.to_string()))?
        .len()
        > MAX_OPERATION_CONTENT_BYTES
    {
        return Err(McpConfigError::InvalidServer(format!(
            "definition exceeds {MAX_OPERATION_CONTENT_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Validates that a shared server can be represented by one application's native format.
pub fn validate_mcp_server_for_app(
    app: &AppType,
    id: &str,
    server: &Value,
) -> Result<(), McpConfigError> {
    validate_mcp_server(id, server)?;
    let contract = mcp_app_contract(app).ok_or_else(|| McpConfigError::UnsupportedApp {
        app_id: app.as_str().to_owned(),
    })?;
    let object = server.as_object().expect("validated server object");
    if !contract.supports_cwd()
        && object
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(|cwd| !cwd.is_empty())
    {
        return Err(McpConfigError::InvalidServer(format!(
            "application '{}' cannot represent 'cwd'",
            app.as_str()
        )));
    }
    if contract.target() == McpConfigTarget::Codex {
        for (key, value) in object {
            if !MANAGED_SERVER_FIELDS.contains(&key.as_str())
                && !MCP_METADATA_FIELDS.contains(&key.as_str())
                && key != "http_headers"
            {
                json_to_toml_item(value)?;
            }
        }
    }
    if contract.target() == McpConfigTarget::Gemini {
        for field in GEMINI_TIMEOUT_FIELDS {
            if let Some(value) = object.get(*field) {
                validate_timeout_value(field, value)?;
            }
        }
    }
    Ok(())
}

/// Compares the managed connection fields of two valid unified servers.
///
/// Application-only extension fields are intentionally ignored so importing
/// the same connection from two applications does not create a false conflict.
/// Transport names are compared using the target application's native fidelity.
pub fn mcp_servers_equivalent(app: &AppType, left: &Value, right: &Value) -> bool {
    comparable_server_fields(app, left)
        .is_some_and(|left| comparable_server_fields(app, right).is_some_and(|right| left == right))
}

/// Reads every valid MCP server from one application live document.
pub fn import_mcp_servers(
    app: &AppType,
    contents: Option<&[u8]>,
) -> Result<Vec<McpImport>, McpConfigError> {
    let target = require_target(app)?;
    validate_document_size(app, contents)?;
    let mut imports = match target {
        McpConfigTarget::Claude => {
            import_json_section(app, contents, "mcpServers", JsonFlavor::Claude)?
        }
        McpConfigTarget::Gemini => {
            import_json_section(app, contents, "mcpServers", JsonFlavor::Gemini)?
        }
        McpConfigTarget::OpenCode => {
            import_json_section(app, contents, "mcp", JsonFlavor::OpenCode)?
        }
        McpConfigTarget::Codex => import_toml_section(app, contents, false)?,
        McpConfigTarget::GrokBuild => import_toml_section(app, contents, true)?,
        McpConfigTarget::Hermes => import_hermes(app, contents)?,
    };
    imports.retain_mut(|entry| {
        if validate_mcp_server(&entry.id, &entry.server).is_err() {
            return false;
        }
        let Some(server) = managed_server_fields(&entry.server) else {
            return false;
        };
        entry.server = server;
        true
    });
    imports.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(imports)
}

/// Reports whether an application live document contains an MCP entry with `id`.
///
/// This inspects the native collection directly, so malformed entries still count as
/// existing and cannot be overwritten during an ownership claim.
pub fn mcp_server_exists(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
) -> Result<bool, McpConfigError> {
    validate_id(id)?;
    let target = require_target(app)?;
    validate_document_size(app, contents)?;
    match target {
        McpConfigTarget::Claude => json_section_contains(app, contents, "mcpServers", id),
        McpConfigTarget::Gemini => json_section_contains(app, contents, "mcpServers", id),
        McpConfigTarget::OpenCode => json_section_contains(app, contents, "mcp", id),
        McpConfigTarget::Codex => toml_section_contains(app, contents, id, false),
        McpConfigTarget::GrokBuild => toml_section_contains(app, contents, id, true),
        McpConfigTarget::Hermes => hermes_section_contains(app, contents, id),
    }
}

/// Captures the raw native entry used to restore application-owned fields.
///
/// Only applications whose disabled state removes the native entry need a
/// snapshot. The selected entry must be an object, but its managed connection
/// fields are deliberately not validated.
pub fn capture_mcp_native_snapshot(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
) -> Result<Option<McpNativeSnapshot>, McpConfigError> {
    validate_id(id)?;
    let target = require_target(app)?;
    validate_document_size(app, contents)?;
    match target {
        McpConfigTarget::Claude => capture_json_snapshot(app, contents, "mcpServers", id, target),
        McpConfigTarget::Gemini => capture_json_snapshot(app, contents, "mcpServers", id, target),
        McpConfigTarget::Codex
        | McpConfigTarget::GrokBuild
        | McpConfigTarget::OpenCode
        | McpConfigTarget::Hermes => Ok(None),
    }
}

/// Projects one application link state into a complete document.
///
/// `Ok(None)` means the live document does not need to be written.
pub fn project_mcp_server(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    projection: McpServerProjection<'_>,
) -> Result<Option<String>, McpConfigError> {
    validate_id(id)?;
    match projection {
        McpServerProjection::Enable(server) | McpServerProjection::Restore { server, .. } => {
            validate_mcp_server_for_app(app, id, server)?;
        }
        McpServerProjection::Disable(server) => validate_mcp_server(id, server)?,
        McpServerProjection::Remove => {}
    }
    let target = require_target(app)?;
    if let McpServerProjection::Restore { snapshot, .. } = projection {
        validate_snapshot(target, snapshot)?;
    }
    validate_document_size(app, contents)?;
    let projected = match target {
        McpConfigTarget::Claude => project_json_section(
            app,
            contents,
            "mcpServers",
            id,
            projection,
            JsonFlavor::Claude,
        ),
        McpConfigTarget::Gemini => project_json_section(
            app,
            contents,
            "mcpServers",
            id,
            projection,
            JsonFlavor::Gemini,
        ),
        McpConfigTarget::OpenCode => {
            project_json_section(app, contents, "mcp", id, projection, JsonFlavor::OpenCode)
        }
        McpConfigTarget::Codex => project_toml_section(app, contents, id, projection, false),
        McpConfigTarget::GrokBuild => project_toml_section(app, contents, id, projection, true),
        McpConfigTarget::Hermes => project_hermes(app, contents, id, projection),
    }?;
    if projected
        .as_ref()
        .is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES)
    {
        return Err(invalid_document(
            app,
            &format!("projected document exceeds {MAX_OPERATION_CONTENT_BYTES} bytes"),
        ));
    }
    Ok(projected)
}

/// Projects several server changes in memory and returns one complete document.
///
/// Hosts can validate the whole batch before performing a single compare-and-swap write.
pub fn project_mcp_servers(
    app: &AppType,
    contents: Option<&[u8]>,
    changes: &[(&str, McpServerProjection<'_>)],
) -> Result<Option<String>, McpConfigError> {
    let original = contents.map(Vec::from);
    let mut projected = original.clone();
    for (id, projection) in changes {
        if let Some(next) = project_mcp_server(app, projected.as_deref(), id, *projection)? {
            projected = Some(next.into_bytes());
        }
    }
    if projected == original {
        return Ok(None);
    }
    projected
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| invalid_document(app, "projected document is not UTF-8"))
}

/// Replaces the complete native MCP collection while preserving target-owned fields
/// on desired entries with the same id.
pub fn replace_mcp_servers(
    app: &AppType,
    contents: Option<&[u8]>,
    servers: &Map<String, Value>,
) -> Result<Option<String>, McpConfigError> {
    let target = require_target(app)?;
    validate_document_size(app, contents)?;
    for (id, server) in servers {
        validate_mcp_server_for_app(app, id, server)?;
    }
    let projected = match target {
        McpConfigTarget::Claude => {
            replace_json_section(app, contents, "mcpServers", JsonFlavor::Claude, servers)?
        }
        McpConfigTarget::Gemini => {
            replace_json_section(app, contents, "mcpServers", JsonFlavor::Gemini, servers)?
        }
        McpConfigTarget::OpenCode => {
            replace_json_section(app, contents, "mcp", JsonFlavor::OpenCode, servers)?
        }
        McpConfigTarget::Codex => replace_toml_section(app, contents, servers, false)?,
        McpConfigTarget::GrokBuild => replace_toml_section(app, contents, servers, true)?,
        McpConfigTarget::Hermes => replace_hermes_section(app, contents, servers)?,
    };
    if projected.len() > MAX_OPERATION_CONTENT_BYTES {
        return Err(invalid_document(
            app,
            &format!("projected document exceeds {MAX_OPERATION_CONTENT_BYTES} bytes"),
        ));
    }
    if contents == Some(projected.as_bytes()) {
        Ok(None)
    } else {
        Ok(Some(projected))
    }
}

fn require_target(app: &AppType) -> Result<McpConfigTarget, McpConfigError> {
    mcp_config_target(app).ok_or_else(|| McpConfigError::UnsupportedApp {
        app_id: app.as_str().to_owned(),
    })
}

fn validate_snapshot(
    target: McpConfigTarget,
    snapshot: &McpNativeSnapshot,
) -> Result<(), McpConfigError> {
    if snapshot.target != target {
        return Err(McpConfigError::InvalidServer(
            "native MCP snapshot belongs to another application".to_owned(),
        ));
    }
    if !matches!(target, McpConfigTarget::Claude | McpConfigTarget::Gemini) {
        return Err(McpConfigError::InvalidServer(
            "this application does not use removable native snapshots".to_owned(),
        ));
    }
    if !json_patch::is_object(&snapshot.entry_json) {
        return Err(McpConfigError::InvalidServer(
            "native MCP snapshot must contain an object".to_owned(),
        ));
    }
    Ok(())
}

fn validate_document_size(app: &AppType, contents: Option<&[u8]>) -> Result<(), McpConfigError> {
    if contents.is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES) {
        Err(invalid_document(
            app,
            &format!("document exceeds {MAX_OPERATION_CONTENT_BYTES} bytes"),
        ))
    } else {
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), McpConfigError> {
    if id != id.trim()
        || id.is_empty()
        || id.len() > MAX_MCP_ID_BYTES
        || id.chars().any(char::is_control)
    {
        Err(McpConfigError::InvalidId(
            "use a non-empty id of at most 128 bytes without surrounding whitespace or control characters"
                .to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    message: &str,
) -> Result<(), McpConfigError> {
    if object
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        Ok(())
    } else {
        Err(McpConfigError::InvalidServer(message.to_owned()))
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<(), McpConfigError> {
    if object.get(key).is_none_or(Value::is_string) {
        Ok(())
    } else {
        Err(McpConfigError::InvalidServer(format!(
            "'{key}' must be a string"
        )))
    }
}

fn string_array(object: &Map<String, Value>, key: &str) -> Result<(), McpConfigError> {
    if object.get(key).is_none_or(|value| {
        value
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string))
    }) {
        Ok(())
    } else {
        Err(McpConfigError::InvalidServer(format!(
            "'{key}' must contain only strings"
        )))
    }
}

fn string_map(object: &Map<String, Value>, key: &str) -> Result<(), McpConfigError> {
    if object.get(key).is_none_or(|value| {
        value
            .as_object()
            .is_some_and(|values| values.values().all(Value::is_string))
    }) {
        Ok(())
    } else {
        Err(McpConfigError::InvalidServer(format!(
            "'{key}' must map strings to strings"
        )))
    }
}

fn reject_fields(
    object: &Map<String, Value>,
    fields: &[&str],
    transport: &str,
) -> Result<(), McpConfigError> {
    if let Some(field) = fields.iter().find(|field| object.contains_key(**field)) {
        Err(McpConfigError::InvalidServer(format!(
            "'{field}' is not valid for '{transport}' definitions"
        )))
    } else {
        Ok(())
    }
}

fn managed_server_fields(server: &Value) -> Option<Value> {
    let object = server.as_object()?;
    let transport = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("stdio");
    let keys: &[&str] = match transport {
        "stdio" => &["command", "args", "env", "cwd"],
        "http" | "sse" => &["url", "headers"],
        _ => return None,
    };
    let mut managed = Map::new();
    managed.insert("type".to_owned(), Value::String(transport.to_owned()));
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        let empty = value.as_array().is_some_and(Vec::is_empty)
            || value.as_object().is_some_and(Map::is_empty)
            || matches!(*key, "cwd") && value.as_str().is_some_and(str::is_empty);
        if !empty {
            managed.insert((*key).to_owned(), value.clone());
        }
    }
    Some(Value::Object(managed))
}

fn comparable_server_fields(app: &AppType, server: &Value) -> Option<Value> {
    let mut managed = managed_server_fields(server)?;
    let contract = mcp_app_contract(app)?;
    let object = managed
        .as_object_mut()
        .expect("managed server is an object");
    if !contract.supports_cwd() {
        object.remove("cwd");
    }
    if matches!(
        app,
        AppType::GrokBuild | AppType::OpenCode | AppType::Hermes
    ) && object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|transport| matches!(transport, "http" | "sse"))
    {
        object.insert("type".to_owned(), Value::String("remote".to_owned()));
    }
    Some(managed)
}

#[derive(Clone, Copy)]
enum JsonFlavor {
    Claude,
    Gemini,
    OpenCode,
}

fn parse_json_root(app: &AppType, contents: Option<&[u8]>) -> Result<Value, McpConfigError> {
    let Some(contents) = contents else {
        return Ok(serde_json::json!({}));
    };
    let text = std::str::from_utf8(contents).map_err(|_| invalid_document(app, "not UTF-8"))?;
    let value: Value =
        json5::from_str(text).map_err(|_| invalid_document(app, "JSON could not be parsed"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(invalid_document(app, "root must be an object"))
    }
}

fn import_json_section(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    flavor: JsonFlavor,
) -> Result<Vec<McpImport>, McpConfigError> {
    let root = parse_json_root(app, contents)?;
    let Some(value) = root.get(section) else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_object()
        .ok_or_else(|| invalid_document(app, &format!("'{section}' must be an object")))?;
    Ok(entries
        .iter()
        .filter_map(|(id, value)| {
            from_json_flavor(flavor, value)
                .ok()
                .map(|(server, enabled)| McpImport {
                    id: id.clone(),
                    server,
                    enabled,
                    native_snapshot: match flavor {
                        JsonFlavor::Claude => capture_json_snapshot(
                            app,
                            contents,
                            section,
                            id,
                            McpConfigTarget::Claude,
                        )
                        .ok()
                        .flatten()
                        .or_else(|| {
                            Some(McpNativeSnapshot {
                                target: McpConfigTarget::Claude,
                                entry_json: value.to_string(),
                            })
                        }),
                        JsonFlavor::Gemini => capture_json_snapshot(
                            app,
                            contents,
                            section,
                            id,
                            McpConfigTarget::Gemini,
                        )
                        .ok()
                        .flatten()
                        .or_else(|| {
                            Some(McpNativeSnapshot {
                                target: McpConfigTarget::Gemini,
                                entry_json: value.to_string(),
                            })
                        }),
                        JsonFlavor::OpenCode => None,
                    },
                })
        })
        .collect())
}

fn json_section_contains(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    id: &str,
) -> Result<bool, McpConfigError> {
    if let Some(contents) = contents {
        let text = std::str::from_utf8(contents).map_err(|_| invalid_document(app, "not UTF-8"))?;
        if let Ok(section) = json_patch::object_entry(text, section) {
            return section
                .map(|section| json_patch::object_entry(&section, id).map(|entry| entry.is_some()))
                .transpose()
                .map(Option::unwrap_or_default)
                .map_err(|message| invalid_document(app, &message));
        }
    }
    let root = parse_json_root(app, contents)?;
    let Some(entries) = root.get(section) else {
        return Ok(false);
    };
    entries
        .as_object()
        .map(|entries| entries.contains_key(id))
        .ok_or_else(|| invalid_document(app, &format!("'{section}' must be an object")))
}

fn capture_json_snapshot(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    id: &str,
    target: McpConfigTarget,
) -> Result<Option<McpNativeSnapshot>, McpConfigError> {
    let Some(contents) = contents else {
        return Ok(None);
    };
    let text = std::str::from_utf8(contents).map_err(|_| invalid_document(app, "not UTF-8"))?;
    let Some(entries) = json_patch::object_entry(text, section)
        .map_err(|message| invalid_document(app, &message))?
    else {
        return Ok(None);
    };
    let Some(entry_json) = json_patch::object_entry(&entries, id)
        .map_err(|message| invalid_document(app, &message))?
    else {
        return Ok(None);
    };
    json_patch::validate_object(&entry_json).map_err(|message| invalid_document(app, &message))?;
    Ok(Some(McpNativeSnapshot { target, entry_json }))
}

fn project_json_section(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    id: &str,
    projection: McpServerProjection<'_>,
    flavor: JsonFlavor,
) -> Result<Option<String>, McpConfigError> {
    ensure_lossless_json_projection(app, contents)?;
    let original = contents
        .map(|contents| std::str::from_utf8(contents))
        .transpose()
        .map_err(|_| invalid_document(app, "not UTF-8"))?
        .unwrap_or("{}");
    let existing_entry = json_patch::object_entry(original, section)
        .map_err(|message| invalid_document(app, &message))?
        .map(|entries| {
            json_patch::object_entry(&entries, id)
                .map_err(|message| invalid_document(app, &message))
        })
        .transpose()?
        .flatten();

    let projected = match projection {
        McpServerProjection::Enable(server) => Some(project_json_entry(
            flavor,
            server,
            existing_entry.as_deref(),
            false,
        )?),
        McpServerProjection::Restore { server, snapshot } => Some(project_json_entry(
            flavor,
            server,
            existing_entry
                .as_deref()
                .or(Some(snapshot.entry_json.as_str())),
            true,
        )?),
        McpServerProjection::Disable(server) if matches!(flavor, JsonFlavor::OpenCode) => {
            let Some(existing) = existing_entry.as_deref() else {
                return Ok(None);
            };
            let mut desired = to_json_flavor(flavor, server, None)?;
            desired
                .as_object_mut()
                .expect("OpenCode projection is an object")
                .insert("enabled".to_owned(), Value::Bool(false));
            Some(merge_json_entry(
                flavor,
                server,
                Some(existing),
                desired,
                false,
            )?)
        }
        McpServerProjection::Disable(_) | McpServerProjection::Remove => None,
    };
    json_patch::replace_nested_object_entry(original, section, id, projected.as_deref())
        .map_err(|message| invalid_document(app, &message))
}

fn project_json_entry(
    flavor: JsonFlavor,
    server: &Value,
    existing: Option<&str>,
    preserve_existing_extensions: bool,
) -> Result<String, McpConfigError> {
    let desired = to_json_flavor(flavor, server, None)?;
    merge_json_entry(
        flavor,
        server,
        existing,
        desired,
        preserve_existing_extensions,
    )
}

fn merge_json_entry(
    flavor: JsonFlavor,
    server: &Value,
    existing: Option<&str>,
    mut desired: Value,
    preserve_existing_extensions: bool,
) -> Result<String, McpConfigError> {
    let desired = desired
        .as_object_mut()
        .expect("native JSON projection is an object");
    let clear_fields: &[&str] = match flavor {
        JsonFlavor::Claude => MANAGED_SERVER_FIELDS,
        JsonFlavor::Gemini => {
            let configured_timeout = server.as_object().is_some_and(|server| {
                GEMINI_TIMEOUT_FIELDS
                    .iter()
                    .any(|field| server.contains_key(*field))
            });
            if preserve_existing_extensions {
                &[
                    "type", "command", "args", "env", "cwd", "url", "httpUrl", "headers",
                ]
            } else if !configured_timeout && existing.is_some() {
                desired.remove("timeout");
                &[
                    "type",
                    "command",
                    "args",
                    "env",
                    "cwd",
                    "url",
                    "httpUrl",
                    "headers",
                    "startup_timeout_sec",
                    "startup_timeout_ms",
                    "tool_timeout_sec",
                    "tool_timeout_ms",
                ]
            } else {
                &[
                    "type",
                    "command",
                    "args",
                    "env",
                    "cwd",
                    "url",
                    "httpUrl",
                    "headers",
                    "timeout",
                    "startup_timeout_sec",
                    "startup_timeout_ms",
                    "tool_timeout_sec",
                    "tool_timeout_ms",
                ]
            }
        }
        JsonFlavor::OpenCode => &[
            "type",
            "command",
            "args",
            "env",
            "environment",
            "cwd",
            "url",
            "headers",
        ],
    };
    if preserve_existing_extensions {
        if let Some(existing) = existing {
            let extension_keys = desired
                .keys()
                .filter(|key| !clear_fields.contains(&key.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            for key in extension_keys {
                if json_patch::object_entry(existing, &key)
                    .map_err(McpConfigError::InvalidServer)?
                    .is_some()
                {
                    desired.remove(&key);
                }
            }
        }
    }
    json_patch::merge_object_fields(existing, clear_fields, desired)
        .map_err(McpConfigError::InvalidServer)
}

fn replace_json_section(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    flavor: JsonFlavor,
    servers: &Map<String, Value>,
) -> Result<String, McpConfigError> {
    ensure_lossless_json_projection(app, contents)?;
    let mut root = parse_json_root(app, contents)?;
    let existing = root
        .get(section)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut ids = servers.keys().collect::<Vec<_>>();
    ids.sort();
    let mut replacement = Map::new();
    for id in ids {
        replacement.insert(
            id.clone(),
            to_json_flavor(flavor, &servers[id], existing.get(id))?,
        );
    }
    let replacement = Value::Object(replacement);
    root.as_object_mut()
        .expect("validated JSON root")
        .insert(section.to_owned(), replacement.clone());
    if let Some(original) = contents {
        let original =
            std::str::from_utf8(original).map_err(|_| invalid_document(app, "not UTF-8"))?;
        json_patch::replace_top_level_value(original, section, &replacement)
            .map_err(|message| invalid_document(app, &message))
    } else {
        pretty_json(root).map_err(|message| invalid_document(app, &message))
    }
}

fn ensure_lossless_json_projection(
    app: &AppType,
    contents: Option<&[u8]>,
) -> Result<(), McpConfigError> {
    let Some(contents) = contents else {
        return Ok(());
    };
    serde_json::from_slice::<Box<serde_json::value::RawValue>>(contents).map_err(|_| {
        invalid_document(
            app,
            "JSON5 syntax cannot be edited without losing comments or formatting",
        )
    })?;
    Ok(())
}

fn from_json_flavor(flavor: JsonFlavor, value: &Value) -> Result<(Value, bool), McpConfigError> {
    let object = value.as_object().ok_or_else(|| {
        McpConfigError::InvalidServer("application entry must be an object".to_owned())
    })?;
    let mut output = object.clone();
    let mut enabled = true;
    match flavor {
        JsonFlavor::Claude => infer_transport(&mut output),
        JsonFlavor::Gemini => {
            if let Some(url) = output.remove("httpUrl") {
                output.insert("url".to_owned(), url);
                output.insert("type".to_owned(), Value::String("http".to_owned()));
            } else {
                infer_transport(&mut output);
            }
        }
        JsonFlavor::OpenCode => {
            enabled = output
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let transport = output
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("local")
                .to_owned();
            match transport.as_str() {
                "local" => {
                    let command = output
                        .remove("command")
                        .and_then(|value| value.as_array().cloned())
                        .ok_or_else(|| {
                            McpConfigError::InvalidServer(
                                "OpenCode local command must be an array".to_owned(),
                            )
                        })?;
                    let Some(program) = command.first().and_then(Value::as_str) else {
                        return Err(McpConfigError::InvalidServer(
                            "OpenCode local command cannot be empty".to_owned(),
                        ));
                    };
                    output.insert("command".to_owned(), Value::String(program.to_owned()));
                    if command.len() > 1 {
                        output.insert("args".to_owned(), Value::Array(command[1..].to_vec()));
                    }
                    if let Some(environment) = output.remove("environment") {
                        output.insert("env".to_owned(), environment);
                    }
                    output.insert("type".to_owned(), Value::String("stdio".to_owned()));
                }
                "remote" => {
                    output.insert("type".to_owned(), Value::String("sse".to_owned()));
                }
                other => {
                    return Err(McpConfigError::InvalidServer(format!(
                        "unsupported OpenCode transport '{other}'"
                    )));
                }
            }
            output.remove("enabled");
        }
    }
    Ok((Value::Object(output), enabled))
}

fn to_json_flavor(
    flavor: JsonFlavor,
    server: &Value,
    existing: Option<&Value>,
) -> Result<Value, McpConfigError> {
    let unified = server_object(server)?;
    match flavor {
        JsonFlavor::Claude => {
            let mut output = existing
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            clear_fields(&mut output, MANAGED_SERVER_FIELDS);
            copy_json_extensions(unified, &mut output, &[]);
            copy_fields(unified, &mut output, MANAGED_SERVER_FIELDS);
            Ok(Value::Object(output))
        }
        JsonFlavor::Gemini => {
            let mut output = existing
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let existing_timeout = output.get("timeout").cloned();
            clear_fields(
                &mut output,
                &[
                    "type", "command", "args", "env", "cwd", "url", "httpUrl", "headers",
                ],
            );
            clear_fields(&mut output, GEMINI_TIMEOUT_FIELDS);
            copy_json_extensions(unified, &mut output, GEMINI_TIMEOUT_FIELDS);
            match unified
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
            {
                "stdio" => copy_fields(unified, &mut output, &["command", "args", "env", "cwd"]),
                "http" => {
                    if let Some(url) = unified.get("url") {
                        output.insert("httpUrl".to_owned(), url.clone());
                    }
                    copy_fields(unified, &mut output, &["headers"]);
                }
                "sse" => copy_fields(unified, &mut output, &["url", "headers"]),
                other => return Err(unsupported_transport(other)),
            }
            let has_configured_timeout = GEMINI_TIMEOUT_FIELDS
                .iter()
                .any(|field| unified.contains_key(*field));
            let timeout = match existing_timeout {
                Some(timeout) if !has_configured_timeout => timeout,
                _ => Value::Number(gemini_timeout_ms(unified).into()),
            };
            output.insert("timeout".to_owned(), timeout);
            Ok(Value::Object(output))
        }
        JsonFlavor::OpenCode => {
            let mut output = existing
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for key in [
                "type",
                "command",
                "args",
                "env",
                "environment",
                "cwd",
                "url",
                "headers",
            ] {
                output.remove(key);
            }
            copy_json_extensions(unified, &mut output, &["environment"]);
            match unified
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
            {
                "stdio" => {
                    output.insert("type".to_owned(), Value::String("local".to_owned()));
                    let mut command = vec![Value::String(
                        unified
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    )];
                    command.extend(
                        unified
                            .get("args")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .cloned(),
                    );
                    output.insert("command".to_owned(), Value::Array(command));
                    if let Some(env) = unified.get("env") {
                        output.insert("environment".to_owned(), env.clone());
                    }
                }
                "http" | "sse" => {
                    output.insert("type".to_owned(), Value::String("remote".to_owned()));
                    copy_fields(unified, &mut output, &["url"]);
                    if let Some(headers) = unified.get("headers") {
                        output.insert("headers".to_owned(), headers.clone());
                    }
                }
                other => return Err(unsupported_transport(other)),
            }
            output.insert("enabled".to_owned(), Value::Bool(true));
            Ok(Value::Object(output))
        }
    }
}

fn copy_json_extensions(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    native_fields: &[&str],
) {
    for (key, value) in source {
        if !MANAGED_SERVER_FIELDS.contains(&key.as_str())
            && !MCP_METADATA_FIELDS.contains(&key.as_str())
            && !native_fields.contains(&key.as_str())
        {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn gemini_timeout_ms(server: &Map<String, Value>) -> u64 {
    const DEFAULT_STARTUP_MS: u64 = 10_000;
    const DEFAULT_TOOL_MS: u64 = 60_000;

    let timeout = numeric_timeout(server.get("timeout"), 1).unwrap_or(0);
    let startup = [
        numeric_timeout(server.get("startup_timeout_sec"), 1_000),
        numeric_timeout(server.get("startup_timeout_ms"), 1),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(DEFAULT_STARTUP_MS);
    let tool = [
        numeric_timeout(server.get("tool_timeout_sec"), 1_000),
        numeric_timeout(server.get("tool_timeout_ms"), 1),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(DEFAULT_TOOL_MS);
    timeout.max(startup).max(tool)
}

fn validate_timeout_value(field: &str, value: &Value) -> Result<(), McpConfigError> {
    let multiplier = if field.ends_with("_sec") { 1_000 } else { 1 };
    if numeric_timeout(Some(value), multiplier).is_some() {
        Ok(())
    } else {
        Err(McpConfigError::InvalidServer(format!(
            "'{field}' must be a non-negative number of whole milliseconds"
        )))
    }
}

fn numeric_timeout(value: Option<&Value>, multiplier: u64) -> Option<u64> {
    value.and_then(|value| {
        if let Some(value) = value.as_u64() {
            return value.checked_mul(multiplier);
        }
        let milliseconds = value.as_f64()? * multiplier as f64;
        (milliseconds.is_finite()
            && milliseconds >= 0.0
            && milliseconds < u64::MAX as f64
            && milliseconds.fract() == 0.0)
            .then_some(milliseconds as u64)
    })
}

fn infer_transport(object: &mut Map<String, Value>) {
    if object.contains_key("type") {
        return;
    }
    let transport = if object.contains_key("command") {
        "stdio"
    } else if object.contains_key("url") {
        "sse"
    } else {
        return;
    };
    object.insert("type".to_owned(), Value::String(transport.to_owned()));
}

fn parse_toml(app: &AppType, contents: Option<&[u8]>) -> Result<DocumentMut, McpConfigError> {
    let Some(contents) = contents else {
        return Ok(DocumentMut::new());
    };
    let text = std::str::from_utf8(contents).map_err(|_| invalid_document(app, "not UTF-8"))?;
    if text.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        text.parse::<DocumentMut>()
            .map_err(|_| invalid_document(app, "TOML could not be parsed"))
    }
}

fn import_toml_section(
    app: &AppType,
    contents: Option<&[u8]>,
    grok: bool,
) -> Result<Vec<McpImport>, McpConfigError> {
    let document = parse_toml(app, contents)?;
    let grok_disabled = if grok {
        grok_disabled_ids(app, &document)?
    } else {
        HashSet::new()
    };
    let mut imports = Vec::new();
    if let Some(entries) = document.get("mcp_servers") {
        let entries = entries
            .as_table_like()
            .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a table"))?;
        append_toml_imports(app, entries, &mut imports)?;
    }

    // Read the historical Codex location without writing it back. A later
    // explicit edit migrates that entry to the official location.
    if !grok {
        if let Some(entries) = document
            .get("mcp")
            .and_then(Item::as_table_like)
            .and_then(|table| table.get("servers"))
            .and_then(Item::as_table_like)
        {
            let mut legacy = Vec::new();
            append_toml_imports(app, entries, &mut legacy)?;
            for entry in legacy {
                if !imports.iter().any(|current| current.id == entry.id) {
                    imports.push(entry);
                }
            }
        }
    }

    for entry in &mut imports {
        if grok_disabled.contains(&entry.id) {
            entry.enabled = false;
        }
        let object = entry.server.as_object_mut().expect("TOML entry object");
        normalize_toml_server(object);
    }
    Ok(imports)
}

fn normalize_toml_server(object: &mut Map<String, Value>) {
    if let Some(headers) = object.remove("http_headers") {
        object.insert("headers".to_owned(), headers);
    }
    if !object.contains_key("type") && object.contains_key("url") {
        object.insert("type".to_owned(), Value::String("http".to_owned()));
    } else {
        infer_transport(object);
    }
}

fn decode_codex_transport_fields(entry: &Map<String, Value>) -> Value {
    let inferred_type = if entry
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|url| !url.trim().is_empty())
    {
        "http"
    } else {
        "stdio"
    };
    let type_value = entry
        .get("type")
        .cloned()
        .unwrap_or_else(|| Value::String(inferred_type.to_owned()));
    let mut fields = Map::new();
    match type_value.as_str() {
        Some("stdio") => {
            if let Some(value) = entry.get("command").and_then(Value::as_str) {
                fields.insert("command".to_owned(), Value::String(value.to_owned()));
            }
            if let Some(values) = entry.get("args").and_then(Value::as_array) {
                let values: Vec<_> = values.iter().filter(|v| v.is_string()).cloned().collect();
                if !values.is_empty() {
                    fields.insert("args".to_owned(), Value::Array(values));
                }
            }
            if let Some(value) = entry.get("cwd").and_then(Value::as_str) {
                if !value.trim().is_empty() {
                    fields.insert("cwd".to_owned(), Value::String(value.to_owned()));
                }
            }
            insert_string_map(&mut fields, "env", entry.get("env"));
        }
        Some("http" | "sse") => {
            if let Some(url) = entry.get("url").and_then(Value::as_str) {
                fields.insert("url".to_owned(), Value::String(url.to_owned()));
            }
            let headers = entry
                .get("http_headers")
                .filter(|value| value.is_object())
                .or_else(|| entry.get("headers"));
            insert_string_map(&mut fields, "headers", headers);
        }
        _ => {}
    }
    // Put the type first, as in the unfiltered codec's input representation.
    let mut output = Map::new();
    output.insert("type".to_owned(), type_value);
    output.extend(fields);
    Value::Object(output)
}

fn insert_string_map(output: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(values) = value.and_then(Value::as_object) {
        let values: Map<_, _> = values
            .iter()
            .filter(|(_, value)| value.is_string())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !values.is_empty() {
            output.insert(key.to_owned(), Value::Object(values));
        }
    }
}

fn toml_section_contains(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    grok: bool,
) -> Result<bool, McpConfigError> {
    let document = parse_toml(app, contents)?;
    if let Some(entries) = document.get("mcp_servers") {
        let entries = entries
            .as_table_like()
            .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a table"))?;
        if entries.contains_key(id) {
            return Ok(true);
        }
    }
    Ok(!grok && legacy_toml_entry(&document, id).is_some())
}

fn append_toml_imports(
    app: &AppType,
    entries: &dyn TableLike,
    output: &mut Vec<McpImport>,
) -> Result<(), McpConfigError> {
    for (id, item) in entries.iter() {
        let mut server = item_to_json(item)
            .and_then(|value| value.as_object().cloned().ok_or(()))
            .map_err(|_| invalid_document(app, "MCP server entries must be tables"))?;
        let enabled = match server.remove("enabled") {
            Some(Value::Bool(enabled)) => enabled,
            Some(_) => {
                return Err(invalid_document(
                    app,
                    "MCP server 'enabled' fields must be booleans",
                ));
            }
            None => true,
        };
        output.push(McpImport {
            id: id.to_owned(),
            server: Value::Object(server),
            enabled,
            native_snapshot: None,
        });
    }
    Ok(())
}

fn project_toml_section(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    projection: McpServerProjection<'_>,
    grok: bool,
) -> Result<Option<String>, McpConfigError> {
    let mut document = parse_toml(app, contents)?;
    let mut changed = false;

    match projection {
        McpServerProjection::Enable(server) | McpServerProjection::Restore { server, .. } => {
            let existing = official_toml_entry(&document, id).cloned().or_else(|| {
                (!grok)
                    .then(|| legacy_toml_entry(&document, id).cloned())
                    .flatten()
            });
            let projected = unified_to_toml_server(server, grok, existing)?;
            let entries = ensure_official_toml_entries(app, &mut document)?;
            entries.insert(id, projected);
            changed = true;
            if grok {
                changed |= set_grok_disabled(app, &mut document, id, false)?;
            }
            if !grok {
                changed |= remove_legacy_toml_entry(&mut document, id);
            }
        }
        McpServerProjection::Disable(server) => {
            if let Some(existing) = official_toml_entry(&document, id).cloned() {
                let mut projected = unified_to_toml_server(server, grok, Some(existing))?;
                set_toml_enabled(&mut projected, false)?;
                ensure_official_toml_entries(app, &mut document)?.insert(id, projected);
                changed = true;
                if grok {
                    changed |= set_grok_disabled(app, &mut document, id, true)?;
                }
            } else if !grok {
                if let Some(existing) = legacy_toml_entry(&document, id).cloned() {
                    let mut projected = unified_to_toml_server(server, grok, Some(existing))?;
                    set_toml_enabled(&mut projected, false)?;
                    document
                        .get_mut("mcp")
                        .and_then(Item::as_table_like_mut)
                        .and_then(|table| table.get_mut("servers"))
                        .and_then(Item::as_table_like_mut)
                        .expect("legacy MCP entry was observed")
                        .insert(id, projected);
                    changed = true;
                }
            }
        }
        McpServerProjection::Remove => {
            if let Some(entries) = document.get_mut("mcp_servers") {
                changed |= entries
                    .as_table_like_mut()
                    .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a table"))?
                    .remove(id)
                    .is_some();
            }
            if !grok {
                changed |= remove_legacy_toml_entry(&mut document, id);
            } else {
                changed |= set_grok_disabled(app, &mut document, id, false)?;
            }
        }
    }

    Ok(changed.then(|| document.to_string()))
}

fn replace_toml_section(
    app: &AppType,
    contents: Option<&[u8]>,
    servers: &Map<String, Value>,
    grok: bool,
) -> Result<String, McpConfigError> {
    let mut document = parse_toml(app, contents)?;
    let mut ids = servers.keys().collect::<Vec<_>>();
    ids.sort();
    let entries = ids
        .into_iter()
        .map(|id| {
            let existing = official_toml_entry(&document, id)
                .cloned()
                .or_else(|| {
                    (!grok)
                        .then(|| legacy_toml_entry(&document, id).cloned())
                        .flatten()
                })
                .filter(|item| item.as_table_like().is_some() || item.as_inline_table().is_some());
            unified_to_toml_server(&servers[id], grok, existing).map(|entry| (id.clone(), entry))
        })
        .collect::<Result<Vec<_>, _>>()?;

    document.as_table_mut().remove("mcp_servers");
    if !entries.is_empty() {
        let mut table = Table::new();
        for (id, entry) in entries {
            table.insert(&id, entry);
        }
        document["mcp_servers"] = Item::Table(table);
    }
    if grok {
        document.as_table_mut().remove("disabled_mcp_servers");
    } else if let Some(mcp) = document.get_mut("mcp").and_then(Item::as_table_like_mut) {
        mcp.remove("servers");
    }
    Ok(document.to_string())
}

fn grok_disabled_ids(
    app: &AppType,
    document: &DocumentMut,
) -> Result<HashSet<String>, McpConfigError> {
    let Some(item) = document.get("disabled_mcp_servers") else {
        return Ok(HashSet::new());
    };
    let array = item.as_array().ok_or_else(|| {
        invalid_document(app, "'disabled_mcp_servers' must be an array of strings")
    })?;
    array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                invalid_document(app, "'disabled_mcp_servers' must contain only strings")
            })
        })
        .collect()
}

fn set_grok_disabled(
    app: &AppType,
    document: &mut DocumentMut,
    id: &str,
    disabled: bool,
) -> Result<bool, McpConfigError> {
    if document.get("disabled_mcp_servers").is_none() {
        if !disabled {
            return Ok(false);
        }
        document["disabled_mcp_servers"] = Item::Value(toml_edit::Value::Array(Array::new()));
    }
    let array = document
        .get_mut("disabled_mcp_servers")
        .and_then(Item::as_array_mut)
        .ok_or_else(|| {
            invalid_document(app, "'disabled_mcp_servers' must be an array of strings")
        })?;
    if array.iter().any(|value| value.as_str().is_none()) {
        return Err(invalid_document(
            app,
            "'disabled_mcp_servers' must contain only strings",
        ));
    }
    if disabled {
        if array.iter().any(|value| value.as_str() == Some(id)) {
            Ok(false)
        } else {
            array.push(id);
            Ok(true)
        }
    } else {
        let mut changed = false;
        loop {
            let position = array.iter().position(|value| value.as_str() == Some(id));
            let Some(position) = position else { break };
            array.remove(position);
            changed = true;
        }
        Ok(changed)
    }
}

fn official_toml_entry<'a>(document: &'a DocumentMut, id: &str) -> Option<&'a Item> {
    document
        .get("mcp_servers")
        .and_then(Item::as_table_like)
        .and_then(|entries| entries.get(id))
}

fn legacy_toml_entry<'a>(document: &'a DocumentMut, id: &str) -> Option<&'a Item> {
    document
        .get("mcp")
        .and_then(Item::as_table_like)
        .and_then(|table| table.get("servers"))
        .and_then(Item::as_table_like)
        .and_then(|entries| entries.get(id))
}

fn ensure_official_toml_entries<'a>(
    app: &AppType,
    document: &'a mut DocumentMut,
) -> Result<&'a mut dyn TableLike, McpConfigError> {
    if let Some(inline) = document
        .get("mcp_servers")
        .and_then(Item::as_inline_table)
        .cloned()
    {
        document["mcp_servers"] = Item::Table(inline.into_table());
    }
    match document.get("mcp_servers") {
        Some(value) if value.as_table_like().is_none() => {
            return Err(invalid_document(app, "'mcp_servers' must be a table"));
        }
        None => document["mcp_servers"] = Item::Table(Table::new()),
        Some(_) => {}
    }
    Ok(document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
        .expect("MCP table initialized"))
}

fn remove_legacy_toml_entry(document: &mut DocumentMut, id: &str) -> bool {
    document
        .get_mut("mcp")
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.get_mut("servers"))
        .and_then(Item::as_table_like_mut)
        .and_then(|entries| entries.remove(id))
        .is_some()
}

fn set_toml_enabled(item: &mut Item, enabled: bool) -> Result<(), McpConfigError> {
    item.as_table_like_mut()
        .ok_or_else(|| {
            McpConfigError::InvalidServer("existing TOML MCP entry must be a table".to_owned())
        })?
        .insert("enabled", toml_edit::value(enabled));
    Ok(())
}

fn unified_to_toml_server(
    server: &Value,
    grok: bool,
    existing: Option<Item>,
) -> Result<Item, McpConfigError> {
    let source = server_object(server)?;
    let mut output = match existing {
        Some(Item::Value(toml_edit::Value::InlineTable(table))) => Item::Table(table.into_table()),
        Some(item) => item,
        None => Item::Table(Table::new()),
    };
    let table = output.as_table_like_mut().ok_or_else(|| {
        McpConfigError::InvalidServer("existing TOML MCP entry must be a table".to_owned())
    })?;
    for key in [
        "type",
        "command",
        "args",
        "env",
        "cwd",
        "url",
        "headers",
        "http_headers",
        "enabled",
    ] {
        table.remove(key);
    }
    for (key, value, replace_extension) in toml_server_fields(source, grok) {
        if replace_extension {
            table.remove(key);
        }
        table.insert(key, json_to_toml_item(value)?);
    }
    Ok(output)
}

fn toml_server_fields(
    source: &Map<String, Value>,
    grok: bool,
) -> impl Iterator<Item = (&str, &Value, bool)> {
    source.iter().filter_map(move |(key, value)| {
        if MANAGED_SERVER_FIELDS.contains(&key.as_str()) && !(grok && key == "type") {
            let target_key = if key == "headers" && !grok {
                "http_headers"
            } else {
                key.as_str()
            };
            Some((target_key, value, false))
        } else if !grok && !MCP_METADATA_FIELDS.contains(&key.as_str()) && key != "http_headers" {
            Some((key.as_str(), value, true))
        } else {
            None
        }
    })
}

fn json_to_toml_item(value: &Value) -> Result<Item, McpConfigError> {
    match value {
        Value::Null => Err(McpConfigError::InvalidServer(
            "null values cannot be written to TOML applications".to_owned(),
        )),
        Value::Bool(value) => Ok(toml_edit::value(*value)),
        Value::String(value) => Ok(toml_edit::value(value.as_str())),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(toml_edit::value(integer))
            } else if value.as_u64().is_some() {
                Err(McpConfigError::InvalidServer(
                    "integer cannot be represented by TOML without loss".to_owned(),
                ))
            } else if let Some(float) = value.as_f64() {
                Ok(toml_edit::value(float))
            } else {
                Err(McpConfigError::InvalidServer(
                    "number cannot be represented by TOML".to_owned(),
                ))
            }
        }
        Value::Array(values) => {
            let mut array = Array::new();
            for value in values {
                array.push(json_to_toml_value(value)?);
            }
            Ok(Item::Value(toml_edit::Value::Array(array)))
        }
        Value::Object(values) => {
            let mut table = Table::new();
            for (key, value) in values {
                table.insert(key, json_to_toml_item(value)?);
            }
            Ok(Item::Table(table))
        }
    }
}

fn json_to_toml_value(value: &Value) -> Result<toml_edit::Value, McpConfigError> {
    match value {
        Value::Object(values) => {
            let mut table = InlineTable::new();
            for (key, value) in values {
                table.insert(key, json_to_toml_value(value)?);
            }
            Ok(toml_edit::Value::InlineTable(table))
        }
        _ => json_to_toml_item(value)?.into_value().map_err(|_| {
            McpConfigError::InvalidServer("nested value cannot be represented by TOML".to_owned())
        }),
    }
}

fn item_to_json(item: &Item) -> Result<Value, ()> {
    match item {
        Item::None => Err(()),
        Item::Value(value) => toml_value_to_json(value),
        Item::Table(table) => table_like_to_json(table),
        Item::ArrayOfTables(tables) => Ok(Value::Array(
            tables
                .iter()
                .map(|table| table_like_to_json(table))
                .collect::<Result<Vec<_>, _>>()?,
        )),
    }
}

fn table_like_to_json(table: &dyn TableLike) -> Result<Value, ()> {
    table
        .iter()
        .map(|(key, item)| Ok((key.to_owned(), item_to_json(item)?)))
        .collect::<Result<Map<_, _>, _>>()
        .map(Value::Object)
}

fn toml_value_to_json(value: &toml_edit::Value) -> Result<Value, ()> {
    match value {
        toml_edit::Value::String(value) => Ok(Value::String(value.value().to_owned())),
        toml_edit::Value::Integer(value) => Ok(Value::Number((*value.value()).into())),
        toml_edit::Value::Float(value) => serde_json::Number::from_f64(*value.value())
            .map(Value::Number)
            .ok_or(()),
        toml_edit::Value::Boolean(value) => Ok(Value::Bool(*value.value())),
        toml_edit::Value::Datetime(value) => Ok(Value::String(value.value().to_string())),
        toml_edit::Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(toml_value_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        toml_edit::Value::InlineTable(values) => values
            .iter()
            .map(|(key, value)| Ok((key.to_owned(), toml_value_to_json(value)?)))
            .collect::<Result<Map<_, _>, _>>()
            .map(Value::Object),
    }
}

fn parse_yaml_root(
    app: &AppType,
    contents: Option<&[u8]>,
) -> Result<serde_yaml::Mapping, McpConfigError> {
    let Some(contents) = contents else {
        return Ok(Default::default());
    };
    let text = std::str::from_utf8(contents).map_err(|_| invalid_document(app, "not UTF-8"))?;
    if has_duplicate_top_level_key(text, "mcp_servers") {
        return Err(invalid_document(
            app,
            "contains duplicate 'mcp_servers' sections",
        ));
    }
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|_| invalid_document(app, "YAML could not be parsed"))?;
    match value {
        serde_yaml::Value::Null => Ok(Default::default()),
        serde_yaml::Value::Mapping(mapping) => Ok(mapping),
        _ => Err(invalid_document(app, "root must be a mapping")),
    }
}

fn import_hermes(app: &AppType, contents: Option<&[u8]>) -> Result<Vec<McpImport>, McpConfigError> {
    let root = parse_yaml_root(app, contents)?;
    let section_key = serde_yaml::Value::String("mcp_servers".to_owned());
    let Some(entries) = root.get(&section_key) else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_mapping()
        .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a mapping"))?;
    let mut imports = Vec::new();
    for (id, server) in entries {
        let Some(id) = id.as_str() else { continue };
        let json = serde_json::to_value(server)
            .map_err(|_| invalid_document(app, "MCP entry cannot be represented as JSON"))?;
        if let Ok(server) = from_hermes(&json) {
            imports.push(McpImport {
                id: id.to_owned(),
                server,
                enabled: json.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                native_snapshot: None,
            });
        }
    }
    Ok(imports)
}

fn hermes_section_contains(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
) -> Result<bool, McpConfigError> {
    let root = parse_yaml_root(app, contents)?;
    let section_key = serde_yaml::Value::String("mcp_servers".to_owned());
    let Some(entries) = root.get(&section_key) else {
        return Ok(false);
    };
    let entries = entries
        .as_mapping()
        .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a mapping"))?;
    Ok(entries.contains_key(serde_yaml::Value::String(id.to_owned())))
}

fn project_hermes(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    projection: McpServerProjection<'_>,
) -> Result<Option<String>, McpConfigError> {
    let original = contents
        .map(|value| std::str::from_utf8(value).map(str::to_owned))
        .transpose()
        .map_err(|_| invalid_document(app, "not UTF-8"))?
        .unwrap_or_default();
    let mut root = parse_yaml_root(app, contents)?;
    let section_key = serde_yaml::Value::String("mcp_servers".to_owned());
    let section_existed = root.contains_key(&section_key);
    let id_key = serde_yaml::Value::String(id.to_owned());
    let existing = root
        .get(&section_key)
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|entries| entries.get(&id_key))
        .cloned();

    let changed = match projection {
        McpServerProjection::Enable(server) | McpServerProjection::Restore { server, .. } => {
            match root.get(&section_key) {
                Some(value) if !value.is_mapping() => {
                    return Err(invalid_document(app, "'mcp_servers' must be a mapping"));
                }
                None => {
                    root.insert(
                        section_key.clone(),
                        serde_yaml::Value::Mapping(Default::default()),
                    );
                }
                Some(_) => {}
            }
            let projected = hermes_projection(app, server, existing.as_ref(), true)?;
            root.get_mut(&section_key)
                .and_then(serde_yaml::Value::as_mapping_mut)
                .expect("MCP mapping initialized")
                .insert(id_key, projected);
            true
        }
        McpServerProjection::Disable(server) => {
            let Some(existing) = existing.as_ref() else {
                return Ok(None);
            };
            let projected = hermes_projection(app, server, Some(existing), false)?;
            root.get_mut(&section_key)
                .and_then(serde_yaml::Value::as_mapping_mut)
                .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a mapping"))?
                .insert(id_key, projected);
            true
        }
        McpServerProjection::Remove => {
            if let Some(entries) = root.get_mut(&section_key) {
                entries
                    .as_mapping_mut()
                    .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a mapping"))?
                    .remove(&id_key)
                    .is_some()
            } else {
                false
            }
        }
    };

    if !changed {
        return Ok(None);
    }
    let projected = replace_yaml_section(
        &original,
        "mcp_servers",
        root.get(&section_key)
            .expect("changed YAML contains MCP section"),
        section_existed,
    )
    .map_err(|message| invalid_document(app, &message))?;
    serde_yaml::from_str::<serde_yaml::Value>(&projected).map_err(|_| {
        invalid_document(
            app,
            "projected MCP section would invalidate references elsewhere in the YAML document",
        )
    })?;
    Ok(Some(projected))
}

fn replace_hermes_section(
    app: &AppType,
    contents: Option<&[u8]>,
    servers: &Map<String, Value>,
) -> Result<String, McpConfigError> {
    let original = contents
        .map(|value| std::str::from_utf8(value).map(str::to_owned))
        .transpose()
        .map_err(|_| invalid_document(app, "not UTF-8"))?
        .unwrap_or_default();
    let root = parse_yaml_root(app, contents)?;
    let section_key = serde_yaml::Value::String("mcp_servers".to_owned());
    let section_existed = root.contains_key(&section_key);
    let existing = root
        .get(&section_key)
        .and_then(serde_yaml::Value::as_mapping)
        .cloned()
        .unwrap_or_default();
    let mut ids = servers.keys().collect::<Vec<_>>();
    ids.sort();
    let mut replacement = serde_yaml::Mapping::new();
    for id in ids {
        let id_key = serde_yaml::Value::String(id.clone());
        replacement.insert(
            id_key.clone(),
            hermes_projection(app, &servers[id], existing.get(&id_key), true)?,
        );
    }
    let projected = replace_yaml_section(
        &original,
        "mcp_servers",
        &serde_yaml::Value::Mapping(replacement),
        section_existed,
    )
    .map_err(|message| invalid_document(app, &message))?;
    serde_yaml::from_str::<serde_yaml::Value>(&projected).map_err(|_| {
        invalid_document(
            app,
            "projected MCP section would invalidate references elsewhere in the YAML document",
        )
    })?;
    Ok(projected)
}

fn hermes_projection(
    app: &AppType,
    server: &Value,
    existing: Option<&serde_yaml::Value>,
    enabled: bool,
) -> Result<serde_yaml::Value, McpConfigError> {
    let existing = existing
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| invalid_document(app, "existing MCP entry is invalid"))?;
    serde_yaml::to_value(to_hermes(server, existing.as_ref(), enabled)?)
        .map_err(|_| invalid_document(app, "MCP entry cannot be written as YAML"))
}

fn from_hermes(value: &Value) -> Result<Value, McpConfigError> {
    let object = server_object(value)?;
    let mut output = object.clone();
    clear_fields(
        &mut output,
        &[
            "type", "command", "args", "env", "cwd", "url", "headers", "enabled",
        ],
    );
    if object.contains_key("command") {
        output.insert("type".to_owned(), Value::String("stdio".to_owned()));
        copy_fields(object, &mut output, &["command", "args", "env"]);
    } else if object.contains_key("url") {
        output.insert("type".to_owned(), Value::String("sse".to_owned()));
        copy_fields(object, &mut output, &["url", "headers"]);
    } else {
        return Err(McpConfigError::InvalidServer(
            "Hermes entry has neither command nor url".to_owned(),
        ));
    }
    Ok(Value::Object(output))
}

fn to_hermes(
    server: &Value,
    existing: Option<&Value>,
    enabled: bool,
) -> Result<Value, McpConfigError> {
    let unified = server_object(server)?;
    let mut output = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    clear_fields(
        &mut output,
        &[
            "type", "command", "args", "env", "cwd", "url", "headers", "enabled",
        ],
    );
    copy_json_extensions(unified, &mut output, &[]);
    match unified
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("stdio")
    {
        "stdio" => copy_fields(unified, &mut output, &["command", "args", "env"]),
        "http" | "sse" => copy_fields(unified, &mut output, &["url", "headers"]),
        other => return Err(unsupported_transport(other)),
    }
    output.insert("enabled".to_owned(), Value::Bool(enabled));
    Ok(Value::Object(output))
}

fn server_object(value: &Value) -> Result<&Map<String, Value>, McpConfigError> {
    value
        .as_object()
        .ok_or_else(|| McpConfigError::InvalidServer("the definition must be an object".to_owned()))
}

fn unsupported_transport(transport: &str) -> McpConfigError {
    McpConfigError::InvalidServer(format!("unsupported transport '{transport}'"))
}

fn clear_fields(target: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        target.remove(*key);
    }
}

fn copy_fields(source: &Map<String, Value>, target: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = source.get(*key) {
            target.insert((*key).to_owned(), value.clone());
        }
    }
}

fn replace_yaml_section(
    raw: &str,
    key: &str,
    value: &serde_yaml::Value,
    section_existed: bool,
) -> Result<String, String> {
    let mut section = serde_yaml::Mapping::new();
    section.insert(serde_yaml::Value::String(key.to_owned()), value.clone());
    let serialized = serde_yaml::to_string(&serde_yaml::Value::Mapping(section))
        .map_err(|error| format!("YAML section could not be serialized: {error}"))?;
    if let Some((start, end)) = yaml_section_range(raw, key) {
        let mut output = String::with_capacity(raw.len() + serialized.len());
        output.push_str(&raw[..start]);
        output.push_str(&serialized);
        output.push_str(&raw[end..]);
        Ok(output)
    } else if section_existed || yaml_uses_flow_root(raw) {
        Err(format!(
            "existing '{key}' section cannot be updated without rewriting unrelated YAML"
        ))
    } else {
        let mut output = raw.to_owned();
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&serialized);
        Ok(output)
    }
}

fn yaml_uses_flow_root(raw: &str) -> bool {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('%'))
        .find_map(|line| {
            let line = line.strip_prefix("---").unwrap_or(line).trim_start();
            (!line.is_empty()).then_some(line.starts_with('{'))
        })
        .unwrap_or(false)
}

fn yaml_section_range(raw: &str, key: &str) -> Option<(usize, usize)> {
    let target = format!("{key}:");
    let mut start = None;
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        let plain = line.trim_end_matches(['\r', '\n']);
        if start.is_none() && top_level_yaml_key(plain) && plain.starts_with(&target) {
            start = Some(offset);
        } else if start.is_some() && top_level_yaml_key(plain) {
            return Some((start.expect("section start"), offset));
        }
        offset += line.len();
    }
    start.map(|start| (start, raw.len()))
}

fn top_level_yaml_key(line: &str) -> bool {
    !line.is_empty()
        && !line.starts_with([' ', '\t', '#', '-'])
        && line.find(':').is_some_and(|colon| {
            let rest = &line[colon + 1..];
            rest.is_empty() || rest.starts_with([' ', '\t', '\r'])
        })
}

fn has_duplicate_top_level_key(raw: &str, key: &str) -> bool {
    let target = format!("{key}:");
    raw.lines()
        .filter(|line| top_level_yaml_key(line) && line.starts_with(&target))
        .take(2)
        .count()
        > 1
}

fn pretty_json(value: Value) -> Result<String, String> {
    serde_json::to_string_pretty(&value)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|error| error.to_string())
}

fn invalid_document(app: &AppType, message: &str) -> McpConfigError {
    McpConfigError::InvalidDocument {
        app_id: app.as_str().to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin_app_registry, AppCapability};
    use serde_json::json;

    #[test]
    fn targets_follow_the_registry_capability_matrix() {
        for descriptor in builtin_app_registry().descriptors() {
            assert_eq!(
                mcp_config_target(descriptor.app()).is_some(),
                descriptor.supports(AppCapability::Mcp),
                "{}",
                descriptor.id()
            );
        }
    }

    #[test]
    fn native_existence_detects_invalid_entries_without_importing_them() {
        let documents: &[(AppType, &[u8])] = &[
            (AppType::Claude, br#"{"mcpServers":{"same":42}}"#),
            (AppType::Gemini, br#"{"mcpServers":{"same":false}}"#),
            (AppType::OpenCode, br#"{"mcp":{"same":[]}}"#),
            (AppType::Codex, b"[mcp_servers.same]\ninvalid = true\n"),
            (AppType::GrokBuild, b"[mcp_servers.same]\ninvalid = true\n"),
            (AppType::Hermes, b"mcp_servers:\n  same: invalid\n"),
        ];
        for (app, contents) in documents {
            assert!(mcp_server_exists(app, Some(contents), "same").unwrap());
            assert!(!mcp_server_exists(app, Some(contents), "other").unwrap());
            assert!(import_mcp_servers(app, Some(contents)).unwrap().is_empty());
        }
    }

    #[test]
    fn adapter_exposes_native_mcp_existence() {
        let adapter = crate::builtin_app_adapter(&AppType::Claude);
        assert!(adapter
            .contains_mcp_server(
                Some(br#"{"mcpServers":{"same":{"command":"npx"}}}"#),
                "same"
            )
            .unwrap());
    }

    #[test]
    fn native_snapshot_capture_keeps_private_fields_from_an_invalid_connection() {
        let snapshot = capture_mcp_native_snapshot(
            &AppType::Claude,
            Some(
                br#"{"mcpServers":{"owned":{"command":42,"trust":"latest","exact":9007199254740993.0},"bad":18446744073709551617}}"#,
            ),
            "owned",
        )
        .expect("capture raw owned entry")
        .expect("Claude entry has a snapshot");
        let encoded = serde_json::to_string(&snapshot).expect("serialize snapshot");
        assert!(encoded.contains(r#""entry":{"#));
        assert!(!encoded.contains("entryJson"));
        assert!(encoded.contains("9007199254740993.0"));
        let snapshot: McpNativeSnapshot =
            serde_json::from_str(&encoded).expect("deserialize snapshot");

        let restored = project_mcp_server(
            &AppType::Claude,
            Some(br#"{"mcpServers":{}}"#),
            "owned",
            McpServerProjection::Restore {
                server: &json!({"command":"npx"}),
                snapshot: &snapshot,
            },
        )
        .expect("restore captured snapshot")
        .expect("document changes");
        assert!(restored.contains(r#""trust":"latest""#));
        assert!(restored.contains(r#""command":"npx""#));
        assert!(restored.contains("9007199254740993.0"));
    }

    #[test]
    fn native_snapshot_capture_rejects_only_an_invalid_selected_shape() {
        assert!(capture_mcp_native_snapshot(
            &AppType::Gemini,
            Some(br#"{"mcpServers":{"owned":false}}"#),
            "owned",
        )
        .is_err());
        assert!(capture_mcp_native_snapshot(
            &AppType::Codex,
            Some(b"mcp_servers = { bad = 1 }"),
            "bad",
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn validates_the_shared_transport_contract() {
        for server in [
            json!({"command": "npx"}),
            json!({"type": "stdio", "command": "uvx", "args": ["server"]}),
            json!({"type": "http", "url": "https://example.com/mcp"}),
            json!({"type": "sse", "url": "https://example.com/sse"}),
        ] {
            validate_mcp_server("server", &server).expect("valid MCP server");
        }
        assert!(validate_mcp_server("server", &json!({"type": "stdio"})).is_err());
        assert!(validate_mcp_server("server", &json!({"type": "grpc"})).is_err());
        assert!(validate_mcp_server("server", &json!({"type": 0, "command": "npx"})).is_err());
        assert!(validate_mcp_server(
            "server",
            &json!({"type":"stdio","command":"npx","url":"https://example.com"})
        )
        .is_err());
        assert!(validate_mcp_server(
            "server",
            &json!({"type":"http","url":"https://example.com","command":"npx"})
        )
        .is_err());
        assert!(validate_mcp_server(" ", &json!({"command": "npx"})).is_err());
        assert!(validate_mcp_server(" server ", &json!({"command": "npx"})).is_err());
        for alias in [
            json!({"command":"npx","environment":{"TOKEN":"secret"}}),
            json!({"type":"http","url":"https://example.com","http_headers":{}}),
            json!({"type":"http","url":"https://example.com","httpUrl":"https://other"}),
        ] {
            assert!(validate_mcp_server("server", &alias).is_err());
        }
        assert_eq!(
            format!(
                "{:?}",
                McpServerProjection::Enable(&json!({"env":{"TOKEN":"secret"}}))
            ),
            "Enable(<redacted>)"
        );
        assert!(validate_mcp_server_for_app(
            &AppType::OpenCode,
            "server",
            &json!({"command":"npx","cwd":"/repo"})
        )
        .is_err());
        validate_mcp_server_for_app(
            &AppType::Codex,
            "server",
            &json!({"command":"npx","cwd":"/repo"}),
        )
        .expect("Codex supports cwd");
        assert!(import_mcp_servers(
            &AppType::Claude,
            Some(br#"{"mcpServers":{"bad":{"type":0,"command":"npx"}}}"#),
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn connection_equivalence_ignores_application_extensions() {
        assert!(mcp_servers_equivalent(
            &AppType::Claude,
            &json!({"command":"npx","args":[],"timeout":30}),
            &json!({"type":"stdio","command":"npx","future":true})
        ));
        assert!(!mcp_servers_equivalent(
            &AppType::Claude,
            &json!({"type":"stdio","command":"npx"}),
            &json!({"type":"stdio","command":"uvx"})
        ));
        assert!(mcp_servers_equivalent(
            &AppType::Hermes,
            &json!({"type":"http","url":"https://example.com"}),
            &json!({"type":"sse","url":"https://example.com"})
        ));
    }

    #[test]
    fn claude_projection_preserves_unrelated_root_data() {
        let projected = project_mcp_server(
            &AppType::Claude,
            Some(
                br#"{"theme":"dark","mcpServers":{"old":{"command":"old"},"new":{"command":"before","future":"keep"}}}"#,
            ),
            "new",
            McpServerProjection::Enable(
                &json!({"type":"stdio","command":"npx","args":["server"],"auth":"do-not-copy"}),
            ),
        )
        .expect("project Claude")
        .expect("changed");
        let root: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["old"]["command"], "old");
        assert_eq!(root["mcpServers"]["new"]["command"], "npx");
        assert_eq!(root["mcpServers"]["new"]["future"], "keep");
        assert_eq!(root["mcpServers"]["new"]["auth"], "do-not-copy");
        let imports = import_mcp_servers(&AppType::Claude, Some(projected.as_bytes())).unwrap();
        assert!(imports
            .iter()
            .find(|entry| entry.id == "new")
            .unwrap()
            .server
            .get("future")
            .is_none());
    }

    #[test]
    fn collection_replacement_preserves_matching_native_fields_and_removes_invalid_entries() {
        let desired = json!({
            "kept": {"command":"new"},
            "added": {"type":"http","url":"https://example.com"}
        });
        let projected = replace_mcp_servers(
            &AppType::Claude,
            Some(
                br#"{"theme":"dark","mcpServers":{"bad":42,"removed":{"command":"old"},"kept":{"command":"old","trust":true}}}"#,
            ),
            desired.as_object().unwrap(),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(root["theme"], "dark");
        assert!(root["mcpServers"].get("bad").is_none());
        assert!(root["mcpServers"].get("removed").is_none());
        assert_eq!(root["mcpServers"]["kept"]["command"], "new");
        assert_eq!(root["mcpServers"]["kept"]["trust"], true);
        assert_eq!(root["mcpServers"]["added"]["url"], "https://example.com");
    }

    #[test]
    fn collection_replacement_conforms_for_every_registered_mcp_target() {
        let desired = json!({"server":{"command":"npx"}});
        for app in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::GrokBuild,
            AppType::OpenCode,
            AppType::Hermes,
        ] {
            let projected = replace_mcp_servers(&app, None, desired.as_object().unwrap())
                .unwrap()
                .unwrap();
            let imports = import_mcp_servers(&app, Some(projected.as_bytes())).unwrap();
            assert_eq!(imports.len(), 1, "{} replacement", app.as_str());
            assert_eq!(imports[0].id, "server");
            assert!(imports[0].enabled);

            let cleared = replace_mcp_servers(&app, Some(projected.as_bytes()), &Map::new())
                .unwrap()
                .unwrap();
            assert!(import_mcp_servers(&app, Some(cleared.as_bytes()))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn batch_projection_is_all_or_nothing_for_the_host() {
        let valid = json!({"command":"new"});
        let invalid = json!({"command":"npx","future":null});
        let result = project_mcp_servers(
            &AppType::Codex,
            Some(b"[mcp_servers.existing]\ncommand = \"old\"\n"),
            &[
                ("existing", McpServerProjection::Enable(&valid)),
                ("invalid", McpServerProjection::Enable(&invalid)),
            ],
        );
        assert!(result.is_err());
    }

    #[test]
    fn gemini_and_opencode_use_native_shapes() {
        let gemini = project_mcp_server(
            &AppType::Gemini,
            Some(
                br#"{"theme":"dark","mcpServers":{"remote":{"httpUrl":"https://old.example","future":"keep","description":"native"}}}"#,
            ),
            "remote",
            McpServerProjection::Enable(&json!({
                "type":"http",
                "url":"https://example.com",
                "headers":{"Authorization":"secret"},
                "startup_timeout_sec":75,
                "future":"replace",
                "name":"ui-only"
            })),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&gemini).unwrap();
        assert_eq!(
            root["mcpServers"]["remote"]["httpUrl"],
            "https://example.com"
        );
        assert!(root["mcpServers"]["remote"].get("type").is_none());
        assert_eq!(root["mcpServers"]["remote"]["future"], "replace");
        assert_eq!(root["mcpServers"]["remote"]["timeout"], 75_000);
        assert!(root["mcpServers"]["remote"].get("name").is_none());
        assert_eq!(root["mcpServers"]["remote"]["description"], "native");

        let fresh_gemini = project_mcp_server(
            &AppType::Gemini,
            None,
            "local",
            McpServerProjection::Enable(&json!({"command":"npx","future":true})),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&fresh_gemini).unwrap();
        assert_eq!(root["mcpServers"]["local"]["future"], true);
        assert_eq!(root["mcpServers"]["local"]["timeout"], 60_000);

        let dual_timeout = project_mcp_server(
            &AppType::Gemini,
            None,
            "local",
            McpServerProjection::Enable(&json!({
                "command":"npx",
                "startup_timeout_sec":1,
                "startup_timeout_ms":120_000,
                "tool_timeout_ms":1
            })),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&dual_timeout).unwrap();
        assert_eq!(root["mcpServers"]["local"]["timeout"], 120_000);

        let opencode = project_mcp_server(
            &AppType::OpenCode,
            Some(br#"{"mcp":{"local":{"type":"local","command":["old"],"timeout":30,"enabled":false}}}"#),
            "local",
            McpServerProjection::Enable(&json!({"type":"stdio","command":"npx","args":["-y"],"env":{"TOKEN":"secret"},"future":"shared"})),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&opencode).unwrap();
        assert_eq!(root["mcp"]["local"]["command"], json!(["npx", "-y"]));
        assert_eq!(root["mcp"]["local"]["environment"]["TOKEN"], "secret");
        assert_eq!(root["mcp"]["local"]["timeout"], 30);
        assert_eq!(root["mcp"]["local"]["future"], "shared");
        assert_eq!(root["mcp"]["local"]["enabled"], true);
        let disabled = project_mcp_server(
            &AppType::OpenCode,
            Some(opencode.as_bytes()),
            "local",
            McpServerProjection::Disable(
                &json!({"type":"stdio","command":"npx","args":["-y"],"env":{"TOKEN":"secret"}}),
            ),
        )
        .unwrap()
        .unwrap();
        let disabled: Value = serde_json::from_str(&disabled).unwrap();
        assert_eq!(disabled["mcp"]["local"]["timeout"], 30);
        assert_eq!(disabled["mcp"]["local"]["enabled"], false);
        let imports = import_mcp_servers(
            &AppType::OpenCode,
            Some(br#"{"mcp":{"local":{"type":"local","command":["old"],"enabled":false}}}"#),
        )
        .unwrap();
        assert!(!imports[0].enabled);
    }

    #[test]
    fn removable_json_entries_restore_only_their_native_fields() {
        let original = br#"{"mcpServers":{"server":{"command":"npx","timeout":30,"trust":true}}}"#;
        let imported = import_mcp_servers(&AppType::Gemini, Some(original)).unwrap();
        let snapshot = imported[0]
            .native_snapshot
            .as_ref()
            .expect("Gemini requires a native snapshot");
        assert!(!format!("{snapshot:?}").contains("timeout"));

        let disabled = project_mcp_server(
            &AppType::Gemini,
            Some(original),
            "server",
            McpServerProjection::Disable(&imported[0].server),
        )
        .unwrap()
        .unwrap();
        assert!(
            serde_json::from_str::<Value>(&disabled).unwrap()["mcpServers"]
                .get("server")
                .is_none()
        );

        let restored = project_mcp_server(
            &AppType::Gemini,
            Some(disabled.as_bytes()),
            "server",
            McpServerProjection::Restore {
                server: &json!({"command":"uvx","trust":false,"fresh":"shared"}),
                snapshot,
            },
        )
        .unwrap()
        .unwrap();
        let restored: Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["mcpServers"]["server"]["command"], "uvx");
        assert_eq!(restored["mcpServers"]["server"]["timeout"], 30);
        assert_eq!(restored["mcpServers"]["server"]["trust"], true);
        assert_eq!(restored["mcpServers"]["server"]["fresh"], "shared");
    }

    #[test]
    fn gemini_restore_keeps_raw_native_timeouts_and_adds_missing_shared_timeout() {
        let original = br#"{"mcpServers":{"server":{"command":"npx","timeout":9007199254740993.0,"startup_timeout_ms":18446744073709551617}}}"#;
        let snapshot = capture_mcp_native_snapshot(&AppType::Gemini, Some(original), "server")
            .unwrap()
            .unwrap();
        let restored = project_mcp_server(
            &AppType::Gemini,
            Some(b"{}"),
            "server",
            McpServerProjection::Restore {
                server: &json!({
                    "command":"uvx",
                    "timeout":1,
                    "startup_timeout_ms":2,
                    "fresh":"shared"
                }),
                snapshot: &snapshot,
            },
        )
        .unwrap()
        .unwrap();
        assert!(restored.contains("9007199254740993.0"));
        assert!(restored.contains("18446744073709551617"));
        let restored_value: Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored_value["mcpServers"]["server"]["command"], "uvx");
        assert_eq!(restored_value["mcpServers"]["server"]["fresh"], "shared");

        let snapshot = capture_mcp_native_snapshot(
            &AppType::Gemini,
            Some(br#"{"mcpServers":{"server":{"command":"npx","trust":true}}}"#),
            "server",
        )
        .unwrap()
        .unwrap();
        let restored = project_mcp_server(
            &AppType::Gemini,
            Some(b"{}"),
            "server",
            McpServerProjection::Restore {
                server: &json!({"command":"uvx","startup_timeout_ms":120_000}),
                snapshot: &snapshot,
            },
        )
        .unwrap()
        .unwrap();
        let restored: Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["mcpServers"]["server"]["timeout"], 120_000);
    }

    #[test]
    fn codex_and_grok_preserve_unrelated_toml() {
        let original = b"model = \"keep\"\n[mcp_servers.old]\ncommand = \"old\"\n[mcp_servers.remote]\nurl = \"https://old.example\"\nenabled = false\nfuture = \"keep\"\nfuture_date = 1979-05-27T07:32:00Z\n";
        let imported = import_mcp_servers(&AppType::Codex, Some(original)).unwrap();
        let imported = imported.iter().find(|item| item.id == "remote").unwrap();
        assert!(!imported.enabled);
        assert!(imported.server.get("enabled").is_none());
        let server = json!({
            "type":"http", "url":"https://example.com/mcp",
            "headers":{"Authorization":"secret"}, "timeout":30
        });
        let codex = project_mcp_server(
            &AppType::Codex,
            Some(original),
            "remote",
            McpServerProjection::Enable(&server),
        )
        .unwrap()
        .unwrap();
        assert!(codex.contains("model = \"keep\""));
        assert!(codex.contains("[mcp_servers.old]"));
        assert!(codex.contains("[mcp_servers.remote.http_headers]"));
        assert!(codex.contains("future = \"keep\""));
        assert!(codex.contains("future_date = 1979-05-27T07:32:00Z"));
        assert!(!codex.contains("future_date = \"1979-05-27T07:32:00Z\""));
        assert!(!codex.contains("enabled = false"));
        assert!(codex.contains("timeout = 30"));

        let fresh_codex = project_mcp_server(
            &AppType::Codex,
            None,
            "local",
            McpServerProjection::Enable(&json!({
                "command":"npx",
                "startup_timeout_sec":15,
                "proxy":{"mode":"auto"},
                "name":"ui-only",
                "description":"ui-only",
                "tags":["ui-only"]
            })),
        )
        .unwrap()
        .unwrap();
        assert!(fresh_codex.contains("startup_timeout_sec = 15"));
        let fresh_codex: DocumentMut = fresh_codex.parse().unwrap();
        let fresh_codex = item_to_json(fresh_codex.get("mcp_servers").unwrap()).unwrap();
        assert_eq!(fresh_codex["local"]["proxy"]["mode"], "auto");
        assert!(fresh_codex["local"].get("name").is_none());
        assert!(fresh_codex["local"].get("description").is_none());
        assert!(fresh_codex["local"].get("tags").is_none());

        let inline = project_mcp_server(
            &AppType::Codex,
            Some(b"model = \"keep\"\nmcp_servers = { local = { command = \"old\", future = \"keep\" } }\n"),
            "local",
            McpServerProjection::Enable(&json!({
                "command":"npx",
                "env":{"TOKEN":"secret"}
            })),
        )
        .unwrap()
        .unwrap();
        let inline: DocumentMut = inline.parse().unwrap();
        assert_eq!(item_to_json(inline.get("model").unwrap()).unwrap(), "keep");
        let inline = item_to_json(inline.get("mcp_servers").unwrap()).unwrap();
        assert_eq!(inline["local"]["command"], "npx");
        assert_eq!(inline["local"]["future"], "keep");
        assert_eq!(inline["local"]["env"]["TOKEN"], "secret");

        assert!(project_mcp_server(
            &AppType::Codex,
            None,
            "invalid-extension",
            McpServerProjection::Enable(&json!({"command":"npx","future":null})),
        )
        .is_err());

        for timeout in [json!("60"), json!(-1), json!(-0.5), json!(60_000.9)] {
            assert!(project_mcp_server(
                &AppType::Gemini,
                None,
                "invalid-timeout",
                McpServerProjection::Enable(&json!({"command":"npx","timeout":timeout})),
            )
            .is_err());
        }
        assert!(project_mcp_server(
            &AppType::Gemini,
            None,
            "fractional-timeout",
            McpServerProjection::Enable(&json!({"command":"npx","tool_timeout_sec":60.0009}),),
        )
        .is_err());
        assert!(project_mcp_server(
            &AppType::Gemini,
            None,
            "overflowing-timeout",
            McpServerProjection::Enable(&json!({"command":"npx","startup_timeout_sec":u64::MAX}),),
        )
        .is_err());

        assert!(project_mcp_server(
            &AppType::Codex,
            None,
            "oversized-integer",
            McpServerProjection::Enable(&json!({"command":"npx","future":u64::MAX})),
        )
        .is_err());

        let disabled = project_mcp_server(
            &AppType::Codex,
            Some(codex.as_bytes()),
            "remote",
            McpServerProjection::Disable(&server),
        )
        .unwrap()
        .unwrap();
        assert!(disabled.contains("enabled = false"));
        assert!(disabled.contains("future_date = 1979-05-27T07:32:00Z"));
        assert!(!disabled.contains("future_date = \"1979-05-27T07:32:00Z\""));

        let grok = project_mcp_server(
            &AppType::GrokBuild,
            Some(original),
            "remote",
            McpServerProjection::Enable(&server),
        )
        .unwrap()
        .unwrap();
        assert!(grok.contains("headers"));
        assert!(!grok.contains("http_headers"));
        assert!(grok.contains("future = \"keep\""));
        let imports = import_mcp_servers(&AppType::GrokBuild, Some(grok.as_bytes())).unwrap();
        assert_eq!(
            imports
                .iter()
                .find(|item| item.id == "remote")
                .unwrap()
                .server["type"],
            "http"
        );

        let grok_disabled = project_mcp_server(
            &AppType::GrokBuild,
            Some(grok.as_bytes()),
            "remote",
            McpServerProjection::Disable(&server),
        )
        .unwrap()
        .unwrap();
        assert!(grok_disabled.contains("disabled_mcp_servers = [\"remote\"]"));
        let imports = import_mcp_servers(&AppType::GrokBuild, Some(grok_disabled.as_bytes()))
            .expect("import disabled Grok server");
        assert!(
            !imports
                .iter()
                .find(|item| item.id == "remote")
                .unwrap()
                .enabled
        );
        let grok_enabled = project_mcp_server(
            &AppType::GrokBuild,
            Some(grok_disabled.as_bytes()),
            "remote",
            McpServerProjection::Enable(&server),
        )
        .unwrap()
        .unwrap();
        assert!(!grok_enabled.contains("\"remote\"]"));
    }

    #[test]
    fn codex_imports_url_only_servers_as_http() {
        let imported = import_mcp_servers(
            &AppType::Codex,
            Some(
                b"[mcp_servers.remote]\nurl = \"https://example.com/mcp\"\n\
                  [mcp_servers.invalid]\ntype = 1\nurl = \"https://invalid.example/mcp\"\n",
            ),
        )
        .unwrap();

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].id, "remote");
        assert_eq!(imported[0].server["type"], "http");
    }

    #[test]
    fn hermes_preserves_extra_fields_and_other_sections() {
        let original = b"# keep\nmodel:\n  default: old\nmcp_servers:\n  remote:\n    url: https://old.example\n    auth: oauth\n    timeout: 30\n    future: keep\n    enabled: false\n";
        let projected = project_mcp_server(
            &AppType::Hermes,
            Some(original),
            "remote",
            McpServerProjection::Enable(&json!({"type":"sse","url":"https://new.example"})),
        )
        .unwrap()
        .unwrap();
        assert!(projected.starts_with("# keep\nmodel:\n  default: old\n"));
        let root: Value = serde_yaml::from_str(&projected).unwrap();
        assert_eq!(root["mcp_servers"]["remote"]["url"], "https://new.example");
        assert_eq!(root["mcp_servers"]["remote"]["auth"], "oauth");
        assert_eq!(root["mcp_servers"]["remote"]["timeout"], 30);
        assert_eq!(root["mcp_servers"]["remote"]["future"], "keep");
        assert_eq!(root["mcp_servers"]["remote"]["enabled"], true);
        let imports = import_mcp_servers(&AppType::Hermes, Some(original)).unwrap();
        assert!(!imports[0].enabled);
        assert!(imports[0].server.get("auth").is_none());
        assert!(imports[0].server.get("timeout").is_none());
        assert!(imports[0].server.get("future").is_none());
        assert!(imports[0].server.get("enabled").is_none());

        let disabled = project_mcp_server(
            &AppType::Hermes,
            Some(original),
            "remote",
            McpServerProjection::Disable(&imports[0].server),
        )
        .unwrap()
        .unwrap();
        let disabled: Value = serde_yaml::from_str(&disabled).unwrap();
        assert_eq!(disabled["mcp_servers"]["remote"]["auth"], "oauth");
        assert_eq!(disabled["mcp_servers"]["remote"]["timeout"], 30);
        assert_eq!(disabled["mcp_servers"]["remote"]["future"], "keep");
        assert_eq!(disabled["mcp_servers"]["remote"]["enabled"], false);

        let fresh = project_mcp_server(
            &AppType::Hermes,
            None,
            "local",
            McpServerProjection::Enable(&json!({"command":"npx","timeout":45,"future":"shared"})),
        )
        .unwrap()
        .unwrap();
        let fresh: Value = serde_yaml::from_str(&fresh).unwrap();
        assert_eq!(fresh["mcp_servers"]["local"]["timeout"], 45);
        assert_eq!(fresh["mcp_servers"]["local"]["future"], "shared");
    }

    #[test]
    fn hermes_rejects_yaml_forms_that_cannot_be_safely_patched() {
        for original in [
            br#""mcp_servers":
  old:
    command: old
"#
            .as_slice(),
            br#"{mcp_servers: {old: {command: old}}, theme: dark}
"#
            .as_slice(),
        ] {
            let error = project_mcp_server(
                &AppType::Hermes,
                Some(original),
                "new",
                McpServerProjection::Enable(&json!({"command":"npx"})),
            )
            .expect_err("unsupported YAML form must not be rewritten");
            assert!(matches!(error, McpConfigError::InvalidDocument { .. }));
        }
    }

    #[test]
    fn hermes_rejects_projection_that_would_break_cross_section_aliases() {
        let original = br#"mcp_servers:
  server: &shared
    command: npx
runtime:
  fallback: *shared
"#;

        let error = project_mcp_server(
            &AppType::Hermes,
            Some(original),
            "server",
            McpServerProjection::Enable(&json!({"command":"uvx"})),
        )
        .expect_err("projection must not leave an alias without its anchor");

        assert!(matches!(error, McpConfigError::InvalidDocument { .. }));
    }

    #[test]
    fn json5_is_importable_but_rejected_for_lossy_projection() {
        let original = br#"{
          // keep this user note
          mcp: { server: { type: 'local', command: ['npx'], }, },
        }"#;
        assert_eq!(
            import_mcp_servers(&AppType::OpenCode, Some(original))
                .unwrap()
                .len(),
            1
        );
        let error = project_mcp_server(
            &AppType::OpenCode,
            Some(original),
            "server",
            McpServerProjection::Enable(&json!({"command":"uvx"})),
        )
        .expect_err("lossy JSON5 rewrite must be rejected");
        assert!(matches!(error, McpConfigError::InvalidDocument { .. }));
    }

    #[test]
    fn projection_rejects_a_document_over_the_shared_limit() {
        let original = serde_json::to_vec(&json!({
            "keep": "x".repeat(MAX_OPERATION_CONTENT_BYTES - 32)
        }))
        .unwrap();
        let error = project_mcp_server(
            &AppType::Claude,
            Some(&original),
            "server",
            McpServerProjection::Enable(&json!({"command":"npx", "future":"x".repeat(512)})),
        )
        .expect_err("oversized projected document");
        assert!(matches!(error, McpConfigError::InvalidDocument { .. }));
    }

    #[test]
    fn public_entries_reject_oversized_input_before_parsing() {
        let oversized = vec![b' '; MAX_OPERATION_CONTENT_BYTES + 1];
        for result in [
            import_mcp_servers(&AppType::Claude, Some(&oversized)).map(|_| ()),
            project_mcp_server(
                &AppType::Claude,
                Some(&oversized),
                "server",
                McpServerProjection::Enable(&json!({"command":"npx"})),
            )
            .map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(McpConfigError::InvalidDocument { .. })
            ));
        }
    }

    #[test]
    fn absent_removal_is_a_noop() {
        assert_eq!(
            project_mcp_server(
                &AppType::Claude,
                Some(br#"{"mcpServers":{}}"#),
                "missing",
                McpServerProjection::Remove,
            )
            .unwrap(),
            None
        );
    }
}
