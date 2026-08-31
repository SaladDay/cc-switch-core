//! Pure MCP configuration adapters shared by CC Switch products.
//!
//! Hosts own paths, locking, persistence, and rollback. This module validates
//! the unified server shape and changes only the MCP section of live config.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike};

use crate::{AppType, MAX_OPERATION_CONTENT_BYTES};

const MAX_MCP_ID_BYTES: usize = 128;
const MANAGED_SERVER_FIELDS: &[&str] = &["type", "command", "args", "env", "cwd", "url", "headers"];

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

impl McpConfigTarget {
    /// Returns the application that owns this document.
    pub fn app(self) -> AppType {
        match self {
            Self::Claude => AppType::Claude,
            Self::Codex => AppType::Codex,
            Self::Gemini => AppType::Gemini,
            Self::GrokBuild => AppType::GrokBuild,
            Self::OpenCode => AppType::OpenCode,
            Self::Hermes => AppType::Hermes,
        }
    }
}

/// One valid server discovered in an application's live MCP document.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpImport {
    pub id: String,
    pub server: Value,
    pub enabled: bool,
}

impl fmt::Debug for McpImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpImport")
            .field("id", &self.id)
            .field("server", &"<redacted>")
            .field("enabled", &self.enabled)
            .finish()
    }
}

/// MCP document or unified-server validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum McpConfigError {
    #[error("application '{app_id}' does not support MCP")]
    UnsupportedApp { app_id: String },
    #[error("MCP server id is invalid: {0}")]
    InvalidId(String),
    #[error("MCP server definition is invalid: {0}")]
    InvalidServer(String),
    #[error("{app_id} MCP configuration is invalid: {message}")]
    InvalidDocument { app_id: String, message: String },
}

/// Returns the live MCP target declared for an application.
pub fn mcp_config_target(app: &AppType) -> Option<McpConfigTarget> {
    match app {
        AppType::Claude => Some(McpConfigTarget::Claude),
        AppType::Codex => Some(McpConfigTarget::Codex),
        AppType::Gemini => Some(McpConfigTarget::Gemini),
        AppType::GrokBuild => Some(McpConfigTarget::GrokBuild),
        AppType::OpenCode => Some(McpConfigTarget::OpenCode),
        AppType::Hermes => Some(McpConfigTarget::Hermes),
        AppType::ClaudeDesktop | AppType::OpenClaw | AppType::Pi => None,
    }
}

/// Validates the small, cross-product MCP server contract.
pub fn validate_mcp_server(id: &str, server: &Value) -> Result<(), McpConfigError> {
    validate_id(id)?;
    let object = server.as_object().ok_or_else(|| {
        McpConfigError::InvalidServer("the definition must be an object".to_owned())
    })?;
    let transport = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("stdio");
    match transport {
        "stdio" => {
            required_string(object, "command", "stdio definitions require command")?;
            string_array(object, "args")?;
            string_map(object, "env")?;
            optional_string(object, "cwd")?;
        }
        "http" | "sse" => {
            required_string(object, "url", "remote definitions require url")?;
            string_map(object, "headers")?;
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

/// Compares the managed connection fields of two valid unified servers.
///
/// Application-only extension fields are intentionally ignored so importing
/// the same connection from two applications does not create a false conflict.
pub fn mcp_servers_equivalent(left: &Value, right: &Value) -> bool {
    managed_server_fields(left)
        .is_some_and(|left| managed_server_fields(right).is_some_and(|right| left == right))
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
    imports.retain(|entry| validate_mcp_server(&entry.id, &entry.server).is_ok());
    imports.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(imports)
}

/// Projects one enabled application link or removal into a complete document.
///
/// Native `enabled = false` entries are represented by [`McpImport::enabled`]
/// during import. Hosts keep that state in their catalog and pass `None` here
/// when an application link is disabled.
///
/// `Ok(None)` means the live document does not need to be written.
pub fn project_mcp_server(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    server: Option<&Value>,
) -> Result<Option<String>, McpConfigError> {
    validate_id(id)?;
    if let Some(server) = server {
        validate_mcp_server(id, server)?;
    }
    let target = require_target(app)?;
    validate_document_size(app, contents)?;
    let projected = match target {
        McpConfigTarget::Claude => {
            project_json_section(app, contents, "mcpServers", id, server, JsonFlavor::Claude)
        }
        McpConfigTarget::Gemini => {
            project_json_section(app, contents, "mcpServers", id, server, JsonFlavor::Gemini)
        }
        McpConfigTarget::OpenCode => {
            project_json_section(app, contents, "mcp", id, server, JsonFlavor::OpenCode)
        }
        McpConfigTarget::Codex => project_toml_section(app, contents, id, server, false),
        McpConfigTarget::GrokBuild => project_toml_section(app, contents, id, server, true),
        McpConfigTarget::Hermes => project_hermes(app, contents, id, server),
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

fn require_target(app: &AppType) -> Result<McpConfigTarget, McpConfigError> {
    mcp_config_target(app).ok_or_else(|| McpConfigError::UnsupportedApp {
        app_id: app.as_str().to_owned(),
    })
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
                })
        })
        .collect())
}

fn project_json_section(
    app: &AppType,
    contents: Option<&[u8]>,
    section: &str,
    id: &str,
    server: Option<&Value>,
    flavor: JsonFlavor,
) -> Result<Option<String>, McpConfigError> {
    let mut root = parse_json_root(app, contents)?;
    let root_object = root.as_object_mut().expect("validated JSON root");
    let existing_entry = root_object
        .get(section)
        .and_then(Value::as_object)
        .and_then(|entries| entries.get(id))
        .cloned();

    if let Some(server) = server {
        match root_object.get(section) {
            Some(value) if !value.is_object() => {
                return Err(invalid_document(
                    app,
                    &format!("'{section}' must be an object"),
                ));
            }
            None => {
                root_object.insert(section.to_owned(), Value::Object(Map::new()));
            }
            Some(_) => {}
        }
        let projected = to_json_flavor(flavor, server, existing_entry.as_ref())?;
        root_object
            .get_mut(section)
            .and_then(Value::as_object_mut)
            .expect("MCP section initialized")
            .insert(id.to_owned(), projected);
    } else {
        let Some(entries) = root_object.get_mut(section) else {
            return Ok(None);
        };
        let entries = entries
            .as_object_mut()
            .ok_or_else(|| invalid_document(app, &format!("'{section}' must be an object")))?;
        if entries.remove(id).is_none() {
            return Ok(None);
        }
    }

    pretty_json(root)
        .map(Some)
        .map_err(|message| invalid_document(app, &message))
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
    let unified = server.as_object().expect("validated server object");
    match flavor {
        JsonFlavor::Claude => {
            let mut output = existing
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            clear_fields(&mut output, MANAGED_SERVER_FIELDS);
            merge_server_extensions(&mut output, unified, &[]);
            copy_fields(unified, &mut output, MANAGED_SERVER_FIELDS);
            Ok(Value::Object(output))
        }
        JsonFlavor::Gemini => {
            let mut output = existing
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            clear_fields(
                &mut output,
                &[
                    "type", "command", "args", "env", "cwd", "url", "httpUrl", "headers",
                ],
            );
            merge_server_extensions(&mut output, unified, &["httpUrl"]);
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
                _ => unreachable!("validated transport"),
            }
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
            for (key, value) in unified {
                if !matches!(
                    key.as_str(),
                    "type" | "command" | "args" | "env" | "cwd" | "url" | "headers"
                ) {
                    output.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
            match unified
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
            {
                "stdio" => {
                    output.insert("type".to_owned(), Value::String("local".to_owned()));
                    let mut command =
                        vec![unified.get("command").expect("validated command").clone()];
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
                    output.insert(
                        "url".to_owned(),
                        unified.get("url").expect("validated url").clone(),
                    );
                    if let Some(headers) = unified.get("headers") {
                        output.insert("headers".to_owned(), headers.clone());
                    }
                }
                _ => unreachable!("validated transport"),
            }
            output.insert("enabled".to_owned(), Value::Bool(true));
            Ok(Value::Object(output))
        }
    }
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
        let object = entry.server.as_object_mut().expect("TOML entry object");
        if let Some(headers) = object.remove("http_headers") {
            object.insert("headers".to_owned(), headers);
        }
        if grok && !object.contains_key("type") && object.contains_key("url") {
            object.insert("type".to_owned(), Value::String("http".to_owned()));
        } else {
            infer_transport(object);
        }
    }
    Ok(imports)
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
        });
    }
    Ok(())
}

fn project_toml_section(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    server: Option<&Value>,
    grok: bool,
) -> Result<Option<String>, McpConfigError> {
    let mut document = parse_toml(app, contents)?;
    let mut changed = false;

    if let Some(server) = server {
        let existing = document
            .get("mcp_servers")
            .and_then(Item::as_table_like)
            .and_then(|entries| entries.get(id))
            .cloned()
            .or_else(|| {
                (!grok)
                    .then(|| {
                        document
                            .get("mcp")
                            .and_then(Item::as_table_like)
                            .and_then(|table| table.get("servers"))
                            .and_then(Item::as_table_like)
                            .and_then(|entries| entries.get(id))
                            .cloned()
                    })
                    .flatten()
            });
        match document.get("mcp_servers") {
            Some(value) if value.as_table_like().is_none() => {
                return Err(invalid_document(app, "'mcp_servers' must be a table"));
            }
            None => document["mcp_servers"] = Item::Table(Table::new()),
            Some(_) => {}
        }
        document
            .get_mut("mcp_servers")
            .and_then(Item::as_table_like_mut)
            .expect("MCP table initialized")
            .insert(id, unified_to_toml_server(server, grok, existing)?);
        changed = true;
    } else if let Some(entries) = document.get_mut("mcp_servers") {
        changed |= entries
            .as_table_like_mut()
            .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a table"))?
            .remove(id)
            .is_some();
    }

    if !grok {
        if let Some(legacy) = document
            .get_mut("mcp")
            .and_then(Item::as_table_like_mut)
            .and_then(|table| table.get_mut("servers"))
            .and_then(Item::as_table_like_mut)
        {
            changed |= legacy.remove(id).is_some();
        }
    }

    Ok(changed.then(|| document.to_string()))
}

fn unified_to_toml_server(
    server: &Value,
    grok: bool,
    existing: Option<Item>,
) -> Result<Item, McpConfigError> {
    let source = server.as_object().expect("validated server object");
    let mut output = existing.unwrap_or_else(|| Item::Table(Table::new()));
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
    for (key, value) in source {
        if !MANAGED_SERVER_FIELDS.contains(&key.as_str())
            && key != "http_headers"
            && key != "enabled"
            && table.get(key).is_none()
        {
            table.insert(key, json_to_toml_item(value)?);
        }
    }
    for (key, value) in source {
        if !MANAGED_SERVER_FIELDS.contains(&key.as_str()) || grok && key == "type" {
            continue;
        }
        let target_key = if key == "headers" && !grok {
            "http_headers"
        } else {
            key
        };
        table.insert(target_key, json_to_toml_item(value)?);
    }
    Ok(output)
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
            let mut table = InlineTable::new();
            for (key, value) in values {
                table.insert(key, json_to_toml_value(value)?);
            }
            Ok(Item::Value(toml_edit::Value::InlineTable(table)))
        }
    }
}

fn json_to_toml_value(value: &Value) -> Result<toml_edit::Value, McpConfigError> {
    json_to_toml_item(value)?.into_value().map_err(|_| {
        McpConfigError::InvalidServer("nested value cannot be represented by TOML".to_owned())
    })
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
        if let Ok(server) = from_hermes(id, &json) {
            imports.push(McpImport {
                id: id.to_owned(),
                server,
                enabled: json.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            });
        }
    }
    Ok(imports)
}

fn project_hermes(
    app: &AppType,
    contents: Option<&[u8]>,
    id: &str,
    server: Option<&Value>,
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

    let changed = if let Some(server) = server {
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
        let existing = existing
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| invalid_document(app, "existing MCP entry is invalid"))?;
        let projected = serde_yaml::to_value(to_hermes(server, existing.as_ref())?)
            .map_err(|_| invalid_document(app, "MCP entry cannot be written as YAML"))?;
        root.get_mut(&section_key)
            .and_then(serde_yaml::Value::as_mapping_mut)
            .expect("MCP mapping initialized")
            .insert(id_key, projected);
        true
    } else if let Some(entries) = root.get_mut(&section_key) {
        entries
            .as_mapping_mut()
            .ok_or_else(|| invalid_document(app, "'mcp_servers' must be a mapping"))?
            .remove(&id_key)
            .is_some()
    } else {
        false
    };

    if !changed {
        return Ok(None);
    }
    replace_yaml_section(
        &original,
        "mcp_servers",
        root.get(&section_key)
            .expect("changed YAML contains MCP section"),
        section_existed,
    )
    .map(Some)
    .map_err(|message| invalid_document(app, &message))
}

fn from_hermes(id: &str, value: &Value) -> Result<Value, McpConfigError> {
    let object = value.as_object().ok_or_else(|| {
        McpConfigError::InvalidServer(format!("Hermes entry '{id}' must be an object"))
    })?;
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
        return Err(McpConfigError::InvalidServer(format!(
            "Hermes entry '{id}' has neither command nor url"
        )));
    }
    Ok(Value::Object(output))
}

fn to_hermes(server: &Value, existing: Option<&Value>) -> Result<Value, McpConfigError> {
    let unified = server.as_object().expect("validated server object");
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
    merge_server_extensions(&mut output, unified, &["enabled"]);
    match unified
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("stdio")
    {
        "stdio" => copy_fields(unified, &mut output, &["command", "args", "env"]),
        "http" | "sse" => copy_fields(unified, &mut output, &["url", "headers"]),
        _ => unreachable!("validated transport"),
    }
    output.insert("enabled".to_owned(), Value::Bool(true));
    Ok(Value::Object(output))
}

fn clear_fields(target: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        target.remove(*key);
    }
}

fn merge_server_extensions(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    excluded: &[&str],
) {
    for (key, value) in source {
        if !MANAGED_SERVER_FIELDS.contains(&key.as_str()) && !excluded.contains(&key.as_str()) {
            target.entry(key.clone()).or_insert_with(|| value.clone());
        }
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
        assert!(validate_mcp_server(" ", &json!({"command": "npx"})).is_err());
        assert!(validate_mcp_server(" server ", &json!({"command": "npx"})).is_err());
    }

    #[test]
    fn connection_equivalence_ignores_application_extensions() {
        assert!(mcp_servers_equivalent(
            &json!({"command":"npx","args":[],"timeout":30}),
            &json!({"type":"stdio","command":"npx","future":true})
        ));
        assert!(!mcp_servers_equivalent(
            &json!({"type":"stdio","command":"npx"}),
            &json!({"type":"stdio","command":"uvx"})
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
            Some(&json!({"type":"stdio","command":"npx","args":["server"]})),
        )
        .expect("project Claude")
        .expect("changed");
        let root: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["old"]["command"], "old");
        assert_eq!(root["mcpServers"]["new"]["command"], "npx");
        assert_eq!(root["mcpServers"]["new"]["future"], "keep");
    }

    #[test]
    fn gemini_and_opencode_use_native_shapes() {
        let gemini = project_mcp_server(
            &AppType::Gemini,
            Some(
                br#"{"theme":"dark","mcpServers":{"remote":{"httpUrl":"https://old.example","future":"keep"}}}"#,
            ),
            "remote",
            Some(&json!({"type":"http","url":"https://example.com","headers":{"Authorization":"secret"}})),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&gemini).unwrap();
        assert_eq!(
            root["mcpServers"]["remote"]["httpUrl"],
            "https://example.com"
        );
        assert!(root["mcpServers"]["remote"].get("type").is_none());
        assert_eq!(root["mcpServers"]["remote"]["future"], "keep");

        let opencode = project_mcp_server(
            &AppType::OpenCode,
            Some(br#"{"mcp":{"local":{"type":"local","command":["old"],"timeout":30,"enabled":false}}}"#),
            "local",
            Some(&json!({"type":"stdio","command":"npx","args":["-y"],"env":{"TOKEN":"secret"}})),
        )
        .unwrap()
        .unwrap();
        let root: Value = serde_json::from_str(&opencode).unwrap();
        assert_eq!(root["mcp"]["local"]["command"], json!(["npx", "-y"]));
        assert_eq!(root["mcp"]["local"]["environment"]["TOKEN"], "secret");
        assert_eq!(root["mcp"]["local"]["timeout"], 30);
        assert_eq!(root["mcp"]["local"]["enabled"], true);
        let imports = import_mcp_servers(
            &AppType::OpenCode,
            Some(br#"{"mcp":{"local":{"type":"local","command":["old"],"enabled":false}}}"#),
        )
        .unwrap();
        assert!(!imports[0].enabled);
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
        let codex = project_mcp_server(&AppType::Codex, Some(original), "remote", Some(&server))
            .unwrap()
            .unwrap();
        assert!(codex.contains("model = \"keep\""));
        assert!(codex.contains("[mcp_servers.old]"));
        assert!(codex.contains("http_headers"));
        assert!(codex.contains("future = \"keep\""));
        assert!(codex.contains("future_date = 1979-05-27T07:32:00Z"));
        assert!(!codex.contains("future_date = \"1979-05-27T07:32:00Z\""));
        assert!(!codex.contains("enabled = false"));

        let grok = project_mcp_server(&AppType::GrokBuild, Some(original), "remote", Some(&server))
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
    }

    #[test]
    fn hermes_preserves_extra_fields_and_other_sections() {
        let original = b"# keep\nmodel:\n  default: old\nmcp_servers:\n  remote:\n    url: https://old.example\n    auth: oauth\n    timeout: 30\n    future: keep\n    enabled: false\n";
        let projected = project_mcp_server(
            &AppType::Hermes,
            Some(original),
            "remote",
            Some(&json!({"type":"sse","url":"https://new.example"})),
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
        assert_eq!(imports[0].server["auth"], "oauth");
        assert_eq!(imports[0].server["timeout"], 30);
        assert_eq!(imports[0].server["future"], "keep");
        assert!(imports[0].server.get("enabled").is_none());

        let removed = project_mcp_server(&AppType::Hermes, Some(original), "remote", None)
            .unwrap()
            .unwrap();
        let restored = project_mcp_server(
            &AppType::Hermes,
            Some(removed.as_bytes()),
            "remote",
            Some(&imports[0].server),
        )
        .unwrap()
        .unwrap();
        let restored: Value = serde_yaml::from_str(&restored).unwrap();
        assert_eq!(restored["mcp_servers"]["remote"]["auth"], "oauth");
        assert_eq!(restored["mcp_servers"]["remote"]["timeout"], 30);
        assert_eq!(restored["mcp_servers"]["remote"]["future"], "keep");
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
                Some(&json!({"command":"npx"})),
            )
            .expect_err("unsupported YAML form must not be rewritten");
            assert!(matches!(error, McpConfigError::InvalidDocument { .. }));
        }
    }

    #[test]
    fn projection_rejects_a_document_over_the_shared_limit() {
        let original = serde_json::to_vec(&json!({
            "keep": "x".repeat(MAX_OPERATION_CONTENT_BYTES - 256)
        }))
        .unwrap();
        let error = project_mcp_server(
            &AppType::Claude,
            Some(&original),
            "server",
            Some(&json!({"command":"npx", "future":"x".repeat(512)})),
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
                Some(&json!({"command":"npx"})),
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
                None,
            )
            .unwrap(),
            None
        );
    }
}
