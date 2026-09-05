//! Native Codex MCP table operations, without connection or catalog policy.

use std::{error::Error, fmt};

use toml_edit::{DocumentMut, Item, Table};

/// A parsed native document. Paths, size limits, synchronization and publication
/// belong to the host. This API does not validate MCP connections or IDs.
/// Existing fields outside the selected tables are retained by the TOML editor.
#[derive(Default)]
pub struct McpDocument {
    document: DocumentMut,
}

impl fmt::Debug for McpDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("McpDocument(<redacted>)")
    }
}

impl std::str::FromStr for McpDocument {
    type Err = McpDocumentParseError;

    fn from_str(contents: &str) -> Result<Self, Self::Err> {
        Self::parse(contents)
    }
}

/// Native TOML diagnostics. Display is intended for the host's parse-error UI
/// and can contain source text; Debug deliberately omits that text.
pub struct McpDocumentParseError(String);

impl fmt::Debug for McpDocumentParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("McpDocumentParseError(<redacted>)")
    }
}

impl fmt::Display for McpDocumentParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for McpDocumentParseError {}

/// Observations from a tolerant removal, for host-owned diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpRemoval {
    pub removed_official: bool,
    pub removed_legacy: bool,
    pub malformed_official_collection: bool,
}

impl McpDocument {
    /// Parses native TOML without treating non-TOML whitespace as an empty file.
    /// Hosts may use `Default` when their own policy accepts a blank document.
    ///
    /// ```
    /// use cc_switch_core::codex::McpDocument;
    /// let mut doc = McpDocument::parse("model = 'keep'\n")?;
    /// doc.upsert_native_server("echo", "command = 'echo'\n")?;
    /// assert!(doc.render().contains("model = 'keep'"));
    /// assert!(doc.remove_server("echo").removed_official);
    /// # Ok::<(), cc_switch_core::codex::McpDocumentParseError>(())
    /// ```
    pub fn parse(contents: &str) -> Result<Self, McpDocumentParseError> {
        Ok(Self {
            document: parse_document(contents)?,
        })
    }

    /// Serializes the current native document. This does not write a file.
    pub fn render(&self) -> String {
        self.document.to_string()
    }

    /// Removes the complete historical `mcp.servers` section, leaving its parent.
    /// This is separate from an upsert so the host explicitly chooses migration.
    pub fn clear_legacy_servers(&mut self) -> bool {
        clear_legacy_servers(&mut self.document)
    }

    /// Removes the complete official collection, including malformed values.
    pub fn clear_servers(&mut self) -> bool {
        self.document.as_table_mut().remove("mcp_servers").is_some()
    }

    /// Replaces the official collection with native entry documents in input
    /// order. Each entry describes a table body, not a unified MCP connection.
    /// Empty input creates an empty table; use `clear_servers` to remove it.
    /// Duplicate IDs keep the last entry. All entries parse before mutation.
    pub fn replace_native_servers<'a>(
        &mut self,
        entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<(), McpDocumentParseError> {
        let mut table = Table::new();
        for (id, contents) in entries {
            table.insert(id, Item::Table(parse_document(contents)?.into_table()));
        }
        self.document["mcp_servers"] = Item::Table(table);
        Ok(())
    }

    /// Replaces one native entry, preserving siblings and existing table style.
    /// A missing or malformed official collection is initialized to a table.
    /// Returns whether a malformed, user-authored collection was replaced.
    /// Native entry parsing completes before mutation; legacy entries are not
    /// touched unless the host calls `clear_legacy_servers` separately.
    pub fn upsert_native_server(
        &mut self,
        id: &str,
        contents: &str,
    ) -> Result<bool, McpDocumentParseError> {
        let entry = parse_document(contents)?.into_table();
        let mut repaired = false;
        if self
            .document
            .get("mcp_servers")
            .and_then(Item::as_table_like)
            .is_none()
        {
            repaired = self
                .document
                .get("mcp_servers")
                .is_some_and(|item| !item.is_none());
            self.document["mcp_servers"] = Item::Table(Table::new());
        }
        self.document
            .get_mut("mcp_servers")
            .and_then(Item::as_table_like_mut)
            .expect("native MCP collection initialized")
            .insert(id, Item::Table(entry));
        Ok(repaired)
    }

    /// Removes one ID from both native locations. Invalid collections are left
    /// untouched; empty tables remain. No connection validation is performed.
    pub fn remove_server(&mut self, id: &str) -> McpRemoval {
        let mut result = McpRemoval::default();
        if let Some(item) = self.document.get_mut("mcp_servers") {
            let user_authored = !item.is_none();
            match item.as_table_like_mut() {
                Some(entries) => result.removed_official = entries.remove(id).is_some(),
                None => result.malformed_official_collection = user_authored,
            }
        }
        result.removed_legacy = remove_legacy_server(&mut self.document, id);
        result
    }
}

fn parse_document(contents: &str) -> Result<DocumentMut, McpDocumentParseError> {
    contents
        .parse()
        .map_err(|error: toml_edit::TomlError| McpDocumentParseError(error.to_string()))
}

pub(crate) fn clear_legacy_servers(document: &mut DocumentMut) -> bool {
    document
        .get_mut("mcp")
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.remove("servers"))
        .is_some()
}

pub(crate) fn remove_legacy_server(document: &mut DocumentMut, id: &str) -> bool {
    document
        .get_mut("mcp")
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.get_mut("servers"))
        .and_then(Item::as_table_like_mut)
        .and_then(|entries| entries.remove(id))
        .is_some()
}
