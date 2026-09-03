use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior};

use crate::{source_fingerprint, SharedStoreError};

/// Canonical MCP catalog table shared by CC Switch products.
pub const MCP_SERVERS_TABLE: &str = "mcp_servers";

const SELECT_MCP_SERVER_FIELDS: &str = "id, name, server_config, description, homepage, docs, tags,
    enabled_claude, enabled_codex, enabled_gemini, enabled_grokbuild, enabled_opencode,
    enabled_hermes";
const MCP_SERVER_FIELD_COUNT: usize = 13;

const CREATE_MCP_SERVERS_TABLE: &str = "CREATE TABLE IF NOT EXISTS main.mcp_servers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    server_config TEXT NOT NULL,
    description TEXT,
    homepage TEXT,
    docs TEXT,
    tags TEXT NOT NULL DEFAULT '[]',
    enabled_claude BOOLEAN NOT NULL DEFAULT 0,
    enabled_codex BOOLEAN NOT NULL DEFAULT 0,
    enabled_gemini BOOLEAN NOT NULL DEFAULT 0,
    enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0,
    enabled_opencode BOOLEAN NOT NULL DEFAULT 0,
    enabled_hermes BOOLEAN NOT NULL DEFAULT 0
)";

const BASE_MCP_SERVER_COLUMNS: &[&str] = &["id", "name", "server_config"];

const MCP_SERVER_COLUMNS: &[McpServerColumn] = &[
    McpServerColumn::primary_text("id"),
    McpServerColumn::required_text("name"),
    McpServerColumn::required_text("server_config"),
    McpServerColumn::optional_text("description"),
    McpServerColumn::optional_text("homepage"),
    McpServerColumn::optional_text("docs"),
    McpServerColumn::defaulted("tags", "TEXT", "TEXT NOT NULL DEFAULT '[]'", "'[]'"),
    McpServerColumn::defaulted(
        "enabled_claude",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
    McpServerColumn::defaulted(
        "enabled_codex",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
    McpServerColumn::defaulted(
        "enabled_gemini",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
    McpServerColumn::defaulted(
        "enabled_grokbuild",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
    McpServerColumn::defaulted(
        "enabled_opencode",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
    McpServerColumn::defaulted(
        "enabled_hermes",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
];

#[derive(Clone, Copy)]
struct McpServerColumn {
    name: &'static str,
    declared_type: &'static str,
    declaration: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    primary_key: i64,
}

impl McpServerColumn {
    const fn primary_text(name: &'static str) -> Self {
        Self {
            name,
            declared_type: "TEXT",
            declaration: "TEXT PRIMARY KEY",
            not_null: false,
            default: None,
            primary_key: 1,
        }
    }

    const fn required_text(name: &'static str) -> Self {
        Self {
            name,
            declared_type: "TEXT",
            declaration: "TEXT NOT NULL",
            not_null: true,
            default: None,
            primary_key: 0,
        }
    }

    const fn optional_text(name: &'static str) -> Self {
        Self {
            name,
            declared_type: "TEXT",
            declaration: "TEXT",
            not_null: false,
            default: None,
            primary_key: 0,
        }
    }

    const fn defaulted(
        name: &'static str,
        declared_type: &'static str,
        declaration: &'static str,
        default: &'static str,
    ) -> Self {
        Self {
            name,
            declared_type,
            declaration,
            not_null: true,
            default: Some(default),
            primary_key: 0,
        }
    }
}

struct ExistingColumn {
    name: String,
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
}

/// An unparsed row from the shared `mcp_servers` table.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerRow {
    pub id: String,
    pub name: String,
    pub server_config: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub docs: Option<String>,
    pub tags: String,
    pub enabled_claude: i64,
    pub enabled_codex: i64,
    pub enabled_gemini: i64,
    pub enabled_grokbuild: i64,
    pub enabled_opencode: i64,
    pub enabled_hermes: i64,
    source_fingerprint: [u8; 32],
}

impl McpServerRow {
    /// Identifies every column value read from the source row, including
    /// unknown future columns, without exposing their contents.
    pub fn source_fingerprint(&self) -> &[u8; 32] {
        &self.source_fingerprint
    }
}

impl fmt::Debug for McpServerRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerRow")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("server_config", &"<redacted>")
            .field("description", &self.description)
            .field("homepage", &self.homepage)
            .field("docs", &self.docs)
            .field("tags", &self.tags)
            .field("enabled_claude", &self.enabled_claude)
            .field("enabled_codex", &self.enabled_codex)
            .field("enabled_gemini", &self.enabled_gemini)
            .field("enabled_grokbuild", &self.enabled_grokbuild)
            .field("enabled_opencode", &self.enabled_opencode)
            .field("enabled_hermes", &self.enabled_hermes)
            .finish()
    }
}

/// Creates or transactionally upgrades the shared `mcp_servers` table.
///
/// Unknown tables, columns, rows, indexes, and triggers are retained. Product
/// migration versions and private MCP state remain owned by the host.
pub fn ensure_mcp_server_schema(connection: &mut Connection) -> Result<(), SharedStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(CREATE_MCP_SERVERS_TABLE, [])?;

    let initial_columns = mcp_server_columns(&transaction)?;
    verify_base_mcp_server_schema(&initial_columns)?;
    verify_mcp_server_primary_key(&transaction, &initial_columns)?;
    for expected in MCP_SERVER_COLUMNS {
        if !initial_columns
            .iter()
            .any(|column| column.name == expected.name)
        {
            transaction.execute_batch(&format!(
                "ALTER TABLE main.mcp_servers ADD COLUMN {} {}",
                expected.name, expected.declaration
            ))?;
        }
    }
    verify_mcp_server_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Reads all shared MCP rows in stable display order.
pub fn read_mcp_server_rows(
    connection: &Connection,
) -> Result<Vec<McpServerRow>, SharedStoreError> {
    let sql = format!(
        "SELECT {SELECT_MCP_SERVER_FIELDS}, mcp_servers.*
         FROM main.mcp_servers AS mcp_servers
         ORDER BY name COLLATE BINARY ASC, id COLLATE BINARY ASC"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map([], mcp_server_from_row)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)?;
    Ok(rows)
}

/// Reads one shared MCP row by its binary identifier.
pub fn read_mcp_server_row(
    connection: &Connection,
    id: &str,
) -> Result<Option<McpServerRow>, SharedStoreError> {
    let sql = format!(
        "SELECT {SELECT_MCP_SERVER_FIELDS}, mcp_servers.*
         FROM main.mcp_servers AS mcp_servers
         WHERE id COLLATE BINARY = ?1"
    );
    connection
        .query_row(&sql, [id], mcp_server_from_row)
        .optional()
        .map_err(SharedStoreError::from)
}

/// Verifies the structural contract required by shared MCP writes.
///
/// Hosts may add columns, indexes, and triggers, but table constraints must
/// retain SQLite's default `ABORT` conflict handling. Trigger bodies keep their
/// own conflict policies because shared writes do not override them.
pub fn verify_mcp_server_write_contract(connection: &Connection) -> Result<(), SharedStoreError> {
    verify_mcp_server_schema(connection)?;
    let table_sql = connection
        .query_row(
            "SELECT sql FROM main.sqlite_schema
             WHERE type = 'table' AND name = 'mcp_servers'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase("mcp_servers must be a table".to_owned())
        })?;
    verify_mcp_server_table_sql(connection, &table_sql)?;
    if has_non_abort_conflict_policy(&table_sql) {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_servers constraints must use ABORT conflict handling".to_owned(),
        ));
    }
    Ok(())
}

fn verify_mcp_server_table_sql(connection: &Connection, sql: &str) -> Result<(), SharedStoreError> {
    let definition = sql.strip_prefix("CREATE TABLE").ok_or_else(|| {
        SharedStoreError::InvalidDatabase("mcp_servers table definition is invalid".to_owned())
    })?;
    let validation_sql = format!("CREATE TABLE IF NOT EXISTS{definition}");
    connection
        .prepare(&validation_sql)
        .map(|_| ())
        .map_err(|_| {
            SharedStoreError::InvalidDatabase("mcp_servers table definition is invalid".to_owned())
        })
}

fn mcp_server_from_row(row: &Row<'_>) -> Result<McpServerRow, rusqlite::Error> {
    Ok(McpServerRow {
        id: row.get(0)?,
        name: row.get(1)?,
        server_config: row.get(2)?,
        description: row.get(3)?,
        homepage: row.get(4)?,
        docs: row.get(5)?,
        tags: row.get(6)?,
        enabled_claude: row.get(7)?,
        enabled_codex: row.get(8)?,
        enabled_gemini: row.get(9)?,
        enabled_grokbuild: row.get(10)?,
        enabled_opencode: row.get(11)?,
        enabled_hermes: row.get(12)?,
        source_fingerprint: source_fingerprint(row, MCP_SERVER_FIELD_COUNT)?,
    })
}

fn has_non_abort_conflict_policy(sql: &str) -> bool {
    let Some(tokens) = sql_tokens(sql.as_bytes()) else {
        return true;
    };
    tokens.iter().enumerate().any(|(index, token)| {
        token.eq_ignore_ascii_case(b"ON")
            && tokens
                .get(index + 1)
                .is_some_and(|token| token.eq_ignore_ascii_case(b"CONFLICT"))
            && !tokens
                .get(index + 2)
                .is_some_and(|token| token.eq_ignore_ascii_case(b"ABORT"))
    })
}

fn sql_tokens(sql: &[u8]) -> Option<Vec<&[u8]>> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < sql.len() {
        match sql[cursor] {
            b'\'' | b'"' | b'`' => {
                let quote = sql[cursor];
                let mut terminated = false;
                cursor += 1;
                while cursor < sql.len() {
                    if sql[cursor] == quote {
                        cursor += 1;
                        if cursor < sql.len() && sql[cursor] == quote {
                            cursor += 1;
                            continue;
                        }
                        terminated = true;
                        break;
                    }
                    cursor += 1;
                }
                if !terminated {
                    return None;
                }
            }
            b'[' => {
                cursor += 1;
                while cursor < sql.len() && sql[cursor] != b']' {
                    cursor += 1;
                }
                if cursor == sql.len() {
                    return None;
                }
                cursor += 1;
            }
            b'-' if sql.get(cursor + 1) == Some(&b'-') => {
                cursor += 2;
                while cursor < sql.len() && sql[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if sql.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < sql.len() && !(sql[cursor] == b'*' && sql[cursor + 1] == b'/') {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(sql.len());
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                let start = cursor;
                cursor += 1;
                while cursor < sql.len()
                    && (sql[cursor].is_ascii_alphanumeric()
                        || matches!(sql[cursor], b'_' | b'$')
                        || sql[cursor] >= 0x80)
                {
                    cursor += 1;
                }
                tokens.push(&sql[start..cursor]);
            }
            _ => cursor += 1,
        }
    }
    Some(tokens)
}

fn mcp_server_columns(connection: &Connection) -> Result<Vec<ExistingColumn>, SharedStoreError> {
    let object_type = connection
        .query_row(
            "SELECT type FROM main.sqlite_schema WHERE name = 'mcp_servers'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if object_type.as_deref() != Some("table") {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_servers must be a table".to_owned(),
        ));
    }

    let mut statement = connection.prepare("PRAGMA main.table_xinfo(mcp_servers)")?;
    let columns = statement
        .query_map([], |row| {
            Ok(ExistingColumn {
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default: row.get(4)?,
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)?;
    Ok(columns)
}

fn verify_base_mcp_server_schema(columns: &[ExistingColumn]) -> Result<(), SharedStoreError> {
    for required in BASE_MCP_SERVER_COLUMNS {
        if !columns.iter().any(|column| column.name == *required) {
            return Err(SharedStoreError::InvalidDatabase(format!(
                "mcp_servers table is missing required column '{required}'"
            )));
        }
    }
    Ok(())
}

fn verify_mcp_server_schema(connection: &Connection) -> Result<(), SharedStoreError> {
    let columns = mcp_server_columns(connection)?;
    verify_mcp_server_primary_key(connection, &columns)?;
    for expected in MCP_SERVER_COLUMNS {
        let actual = columns
            .iter()
            .find(|column| column.name == expected.name)
            .ok_or_else(|| {
                SharedStoreError::InvalidDatabase(format!(
                    "mcp_servers table is missing required column '{}'",
                    expected.name
                ))
            })?;
        let default_matches = match (&actual.default, expected.default) {
            (None, None) => true,
            (Some(actual), Some(expected)) => actual.trim_matches(['(', ')']) == expected,
            _ => false,
        };
        if actual.hidden != 0
            || !actual
                .declared_type
                .eq_ignore_ascii_case(expected.declared_type)
            || actual.not_null != expected.not_null
            || actual.primary_key != expected.primary_key
            || !default_matches
        {
            return Err(SharedStoreError::InvalidDatabase(format!(
                "mcp_servers column '{}' does not match the shared contract",
                expected.name
            )));
        }
    }
    Ok(())
}

fn verify_mcp_server_primary_key(
    connection: &Connection,
    columns: &[ExistingColumn],
) -> Result<(), SharedStoreError> {
    let primary_key = columns
        .iter()
        .filter(|column| column.primary_key > 0)
        .collect::<Vec<_>>();
    if primary_key.len() != 1 || primary_key[0].name != "id" || primary_key[0].primary_key != 1 {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_servers primary key must be exactly (id)".to_owned(),
        ));
    }

    let primary_key_index = connection
        .query_row(
            "SELECT name FROM pragma_index_list('mcp_servers', 'main')
             WHERE origin = 'pk' AND partial = 0",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(
                "mcp_servers table has no canonical primary key index".to_owned(),
            )
        })?;
    let indexed_column = connection.query_row(
        "SELECT name, \"desc\", coll FROM pragma_index_xinfo(?1, 'main')
         WHERE key = 1 ORDER BY seqno",
        [primary_key_index],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    if indexed_column != (Some("id".to_owned()), 0, "BINARY".to_owned()) {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_servers primary key must use binary id ordering".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedDatabase;

    fn test_database() -> (tempfile::TempDir, SharedDatabase) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        (directory, database)
    }

    #[test]
    fn creates_canonical_schema_without_owning_product_version() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .pragma_update(None, "user_version", 37)
            .expect("set product version");

        ensure_mcp_server_schema(&mut connection).expect("initialize MCP schema");

        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("read product version"),
            37
        );
        let columns = mcp_server_columns(&connection).expect("read MCP schema");
        assert_eq!(columns.len(), MCP_SERVER_COLUMNS.len());
        verify_mcp_server_schema(&connection).expect("verify MCP schema");
    }

    #[test]
    fn upgrades_known_columns_and_preserves_host_extensions() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE mcp_servers (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    server_config TEXT NOT NULL,
                    host_value TEXT
                 );
                 CREATE TABLE host_mcp_state (value TEXT);
                 CREATE INDEX host_mcp_name ON mcp_servers(host_value);
                 CREATE TRIGGER host_mcp_insert AFTER INSERT ON mcp_servers BEGIN
                    INSERT INTO host_mcp_state VALUES (NEW.host_value);
                 END;
                 INSERT INTO mcp_servers (id, name, server_config, host_value)
                 VALUES ('server', 'Server', '{\"token\":\"secret\"}', 'keep');",
            )
            .expect("create host MCP schema");

        ensure_mcp_server_schema(&mut connection).expect("upgrade MCP schema");

        let row = read_mcp_server_row(&connection, "server")
            .expect("read MCP row")
            .expect("MCP row exists");
        assert_eq!(row.server_config, "{\"token\":\"secret\"}");
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_value FROM mcp_servers WHERE id = 'server'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read host extension"),
            "keep"
        );
        connection
            .execute(
                "INSERT INTO mcp_servers (id, name, server_config, host_value)
                 VALUES ('second', 'Second', '{}', 'trigger-kept')",
                [],
            )
            .expect("exercise retained trigger");
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM host_mcp_state WHERE value = 'trigger-kept'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read retained trigger output"),
            "trigger-kept"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM main.sqlite_schema
                     WHERE type = 'index' AND name = 'host_mcp_name'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("inspect retained index"),
            1
        );
    }

    #[test]
    fn reads_raw_rows_in_stable_binary_order() {
        let (_directory, database) = test_database();
        database
            .ensure_mcp_server_schema()
            .expect("initialize MCP schema");
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "INSERT INTO mcp_servers
                    (id, name, server_config, tags, enabled_claude, enabled_codex)
                 VALUES
                    ('lower', 'server', '{\"token\":\"one\"}', '[\"one\"]', 2, -1),
                    ('upper', 'Server', '{\"token\":\"two\"}', '[\"two\"]', 0, 1);",
            )
            .expect("insert MCP rows");

        let rows = read_mcp_server_rows(&connection).expect("read MCP rows");
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["upper", "lower"]
        );
        assert_eq!(rows[1].enabled_claude, 2);
        assert_eq!(rows[1].enabled_codex, -1);
        assert_eq!(rows[1].tags, "[\"one\"]");
        let debug = format!("{:?}", rows[1]);
        assert!(!debug.contains("token"));
        assert!(!debug.contains("one\"}"));
        assert_eq!(
            read_mcp_server_row(&connection, "upper")
                .expect("read MCP row")
                .expect("MCP row exists"),
            rows[0]
        );
        assert!(read_mcp_server_row(&connection, "Upper")
            .expect("read binary mismatch")
            .is_none());
    }

    #[test]
    fn source_fingerprint_covers_unknown_host_columns() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP schema");
        connection
            .execute_batch(
                "ALTER TABLE mcp_servers ADD COLUMN host_secret TEXT;
                 INSERT INTO mcp_servers (id, name, server_config, host_secret)
                 VALUES ('server', 'Server', '{}', 'first');",
            )
            .expect("add host extension");
        let before = read_mcp_server_row(&connection, "server")
            .expect("read MCP row")
            .expect("MCP row exists");

        connection
            .execute(
                "UPDATE mcp_servers SET host_secret = 'second' WHERE id = 'server'",
                [],
            )
            .expect("change host extension");
        let after = read_mcp_server_row(&connection, "server")
            .expect("read changed MCP row")
            .expect("MCP row exists");

        assert_ne!(before.source_fingerprint(), after.source_fingerprint());
        assert!(!format!("{before:?}").contains("first"));
    }

    #[test]
    fn invalid_primary_key_is_rejected_before_schema_upgrade() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE mcp_servers (
                    id TEXT PRIMARY KEY COLLATE NOCASE,
                    name TEXT NOT NULL,
                    server_config TEXT NOT NULL
                 );",
            )
            .expect("create incompatible MCP schema");

        let error = ensure_mcp_server_schema(&mut connection)
            .expect_err("reject incompatible MCP primary key");
        assert!(error.to_string().contains("binary id ordering"));
        assert_eq!(
            mcp_server_columns(&connection)
                .expect("read unchanged schema")
                .len(),
            3
        );
    }

    #[test]
    fn failed_final_validation_rolls_back_the_entire_upgrade() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "PRAGMA user_version = 41;
                 CREATE TABLE mcp_servers (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    server_config TEXT NOT NULL,
                    tags INTEGER NOT NULL DEFAULT '[]',
                    host_value TEXT
                 );
                 CREATE INDEX host_mcp_name ON mcp_servers(host_value);
                 CREATE TRIGGER host_mcp_update AFTER UPDATE ON mcp_servers BEGIN
                    SELECT NEW.host_value;
                 END;
                 INSERT INTO mcp_servers
                    (id, name, server_config, tags, host_value)
                 VALUES ('server', 'Server', '{}', '[]', 'keep');",
            )
            .expect("create partially incompatible MCP schema");

        let error =
            ensure_mcp_server_schema(&mut connection).expect_err("reject incompatible MCP column");

        assert!(error.to_string().contains("column 'tags'"));
        assert_eq!(
            mcp_server_columns(&connection)
                .expect("read rolled-back schema")
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "name", "server_config", "tags", "host_value"]
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_value FROM mcp_servers WHERE id = 'server'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read preserved row"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM main.sqlite_schema
                     WHERE name IN ('host_mcp_name', 'host_mcp_update')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("inspect retained host objects"),
            2
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("read product version"),
            41
        );
    }

    #[test]
    fn write_contract_accepts_host_extensions_and_trigger_policies() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP schema");
        connection
            .execute_batch(
                "ALTER TABLE mcp_servers
                    ADD COLUMN host_note TEXT DEFAULT 'ON CONFLICT REPLACE';
                 CREATE TABLE host_mcp_state (
                    value TEXT UNIQUE ON CONFLICT IGNORE
                 );
                 CREATE INDEX host_mcp_name ON mcp_servers(name);
                 CREATE TRIGGER host_mcp_insert AFTER INSERT ON mcp_servers BEGIN
                    INSERT OR IGNORE INTO host_mcp_state VALUES (NEW.name);
                 END;
                 CREATE TABLE alternate_mcp_servers (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    server_config TEXT NOT NULL,
                    description TEXT,
                    homepage TEXT,
                    docs TEXT,
                    tags TEXT NOT NULL DEFAULT '[]',
                    enabled_claude BOOLEAN NOT NULL DEFAULT 0,
                    enabled_codex BOOLEAN NOT NULL DEFAULT 0,
                    enabled_gemini BOOLEAN NOT NULL DEFAULT 0,
                    enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0,
                    enabled_opencode BOOLEAN NOT NULL DEFAULT 0,
                    enabled_hermes BOOLEAN NOT NULL DEFAULT 0,
                    ON$ INTEGER,
                    CONFLICT$ INTEGER,
                    REPLACE$ INTEGER,
                    -- ON CONFLICT REPLACE is still part of this comment\r
                    CHECK (ON$ + CONFLICT$ + REPLACE$ >= 0)
                 );",
            )
            .expect("add host extensions");

        verify_mcp_server_write_contract(&connection).expect("accept compatible host extensions");

        connection
            .execute_batch(
                "DROP TABLE mcp_servers;
                 ALTER TABLE alternate_mcp_servers RENAME TO mcp_servers;",
            )
            .expect("install compatible lexical edge-case schema");
        verify_mcp_server_write_contract(&connection)
            .expect("ignore comments and complete bare identifiers");
    }

    #[test]
    fn write_contract_rejects_non_abort_table_conflict_policies() {
        for policy in ["ROLLBACK", "FAIL", "IGNORE", "REPLACE"] {
            let connection = Connection::open_in_memory().expect("open database");
            connection
                .execute_batch(&format!(
                    "CREATE TABLE mcp_servers (
                        id TEXT PRIMARY KEY ON CONFLICT {policy},
                        name TEXT NOT NULL,
                        server_config TEXT NOT NULL,
                        description TEXT,
                        homepage TEXT,
                        docs TEXT,
                        tags TEXT NOT NULL DEFAULT '[]',
                        enabled_claude BOOLEAN NOT NULL DEFAULT 0,
                        enabled_codex BOOLEAN NOT NULL DEFAULT 0,
                        enabled_gemini BOOLEAN NOT NULL DEFAULT 0,
                        enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0,
                        enabled_opencode BOOLEAN NOT NULL DEFAULT 0,
                        enabled_hermes BOOLEAN NOT NULL DEFAULT 0
                     );"
                ))
                .expect("create incompatible MCP table");

            let error = verify_mcp_server_write_contract(&connection)
                .expect_err("reject non-ABORT conflict policy");
            assert!(matches!(
                error,
                SharedStoreError::InvalidDatabase(message)
                    if message == "mcp_servers constraints must use ABORT conflict handling"
            ));
        }
    }

    #[test]
    fn conflict_policy_scanner_handles_valid_sql_edges() {
        assert!(!has_non_abort_conflict_policy(
            "CREATE TABLE t (id TEXT PRIMARY KEY ON /* keep */ CONFLICT ABORT,
             note TEXT DEFAULT 'ON CONFLICT REPLACE', \"ON CONFLICT FAIL\" TEXT)"
        ));
    }

    #[test]
    fn write_contract_rejects_corrupt_table_definition() {
        let connection = Connection::open_in_memory().expect("open database");
        connection
            .execute_batch(CREATE_MCP_SERVERS_TABLE)
            .expect("create canonical MCP table");
        connection
            .execute_batch(
                "PRAGMA writable_schema = ON;
                 UPDATE sqlite_schema
                    SET sql = substr(sql, 1, length(sql) - 1)
                  WHERE type = 'table' AND name = 'mcp_servers';
                 PRAGMA writable_schema = OFF;",
            )
            .expect("corrupt stored table definition");

        let error = verify_mcp_server_write_contract(&connection)
            .expect_err("reject corrupt table definition");
        assert!(matches!(
            error,
            SharedStoreError::InvalidDatabase(message)
                if message == "mcp_servers table definition is invalid"
        ));
    }
}
