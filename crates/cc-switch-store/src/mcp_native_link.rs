use std::fmt;

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::{mcp::has_non_abort_conflict_policy, SharedStoreError};

/// Canonical ownership and native-snapshot table shared by CC Switch products.
pub const MCP_NATIVE_LINKS_TABLE: &str = "mcp_native_links";

const CREATE_MCP_NATIVE_LINKS_TABLE: &str = "CREATE TABLE IF NOT EXISTS main.mcp_native_links (
    server_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    native_snapshot TEXT,
    PRIMARY KEY (server_id, app_id)
)";

const MCP_NATIVE_LINK_DELETE_TRIGGER: &str = "cc_switch_mcp_native_links_after_server_delete";
const CREATE_MCP_NATIVE_LINK_DELETE_TRIGGER: &str =
    "CREATE TRIGGER cc_switch_mcp_native_links_after_server_delete
     AFTER DELETE ON main.mcp_servers
     BEGIN
         DELETE FROM mcp_native_links WHERE server_id COLLATE BINARY = OLD.id;
     END";

#[derive(Clone)]
struct ExistingColumn {
    name: String,
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
}

/// Raw ownership state for one shared MCP server and application.
#[derive(Clone, PartialEq, Eq)]
pub struct McpNativeLinkRow {
    pub server_id: String,
    pub app_id: String,
    pub native_snapshot: Option<String>,
}

impl fmt::Debug for McpNativeLinkRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpNativeLinkRow")
            .field("server_id", &self.server_id)
            .field("app_id", &self.app_id)
            .field(
                "native_snapshot",
                &self.native_snapshot.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Creates and validates the shared MCP native-link table.
///
/// Unknown columns, indexes, and triggers are retained. Links whose shared
/// catalog row no longer exists are removed without changing `user_version`.
/// When the table is first introduced, enabled legacy catalog rows are claimed
/// so an upgrade does not silently stop managing their live entries.
pub fn ensure_mcp_native_link_schema(connection: &mut Connection) -> Result<(), SharedStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let table_existed = native_link_table_exists(&transaction)?;
    transaction.execute(CREATE_MCP_NATIVE_LINKS_TABLE, [])?;
    verify_mcp_native_link_schema(&transaction)?;
    ensure_delete_trigger(&transaction)?;
    if !table_existed {
        claim_enabled_legacy_links(&transaction)?;
    }
    transaction
        .execute(
            "DELETE FROM main.mcp_native_links
             WHERE NOT EXISTS (
                 SELECT 1 FROM main.mcp_servers
                  WHERE mcp_servers.id COLLATE BINARY = mcp_native_links.server_id
             )",
            [],
        )
        .map_err(|error| redact_native_link_write_error(error, transaction.is_autocommit()))?;
    let orphaned = transaction.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM main.mcp_native_links
              WHERE NOT EXISTS (
                  SELECT 1 FROM main.mcp_servers
                   WHERE mcp_servers.id COLLATE BINARY = mcp_native_links.server_id
              )
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if orphaned {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_native_links orphan cleanup was suppressed".to_owned(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

fn native_link_table_exists(connection: &Connection) -> Result<bool, SharedStoreError> {
    connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM main.sqlite_master
                  WHERE type = 'table' AND name = ?1
             )",
            [MCP_NATIVE_LINKS_TABLE],
            |row| row.get(0),
        )
        .map_err(SharedStoreError::from)
}

fn claim_enabled_legacy_links(connection: &Connection) -> Result<(), SharedStoreError> {
    const ENABLED_APPS: &[(&str, &str)] = &[
        ("claude", "enabled_claude"),
        ("codex", "enabled_codex"),
        ("gemini", "enabled_gemini"),
        ("grokbuild", "enabled_grokbuild"),
        ("opencode", "enabled_opencode"),
        ("hermes", "enabled_hermes"),
    ];
    for (app_id, column) in ENABLED_APPS {
        connection
            .execute(
                &format!(
                    "INSERT INTO main.mcp_native_links (server_id, app_id, native_snapshot)
                     SELECT id, ?1, NULL FROM main.mcp_servers WHERE {column} <> 0"
                ),
                [app_id],
            )
            .map_err(|error| redact_native_link_write_error(error, connection.is_autocommit()))?;
    }
    Ok(())
}

/// Reads one native link using binary server and application identifiers.
///
/// A returned row with a `None` snapshot is distinct from an absent row: the
/// former still records that the application owns the native entry.
pub fn read_mcp_native_link(
    connection: &Connection,
    server_id: &str,
    app_id: &str,
) -> Result<Option<McpNativeLinkRow>, SharedStoreError> {
    connection
        .query_row(
            "SELECT server_id, app_id, native_snapshot
               FROM main.mcp_native_links
              WHERE server_id COLLATE BINARY = ?1
                AND app_id COLLATE BINARY = ?2",
            params![server_id, app_id],
            mcp_native_link_from_row,
        )
        .optional()
        .map_err(SharedStoreError::from)
}

/// Records application ownership and replaces its optional native snapshot.
///
/// The caller supplies the same immediate transaction used for the catalog
/// change, so ownership cannot commit independently from the shared row.
pub fn upsert_mcp_native_link(
    transaction: &mut Transaction<'_>,
    server_id: &str,
    app_id: &str,
    native_snapshot: Option<&str>,
) -> Result<(), SharedStoreError> {
    prepare_mcp_native_link_write(transaction)?;
    let parent_exists = transaction.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM main.mcp_servers WHERE id COLLATE BINARY = ?1
         )",
        [server_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !parent_exists {
        return Err(SharedStoreError::InvalidDatabase(
            "an MCP native link requires an existing shared server".to_owned(),
        ));
    }
    execute_mcp_native_link_write(transaction, |connection| {
        connection.execute(
            "INSERT INTO main.mcp_native_links (server_id, app_id, native_snapshot)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(server_id, app_id) DO UPDATE
                 SET native_snapshot = excluded.native_snapshot",
            params![server_id, app_id, native_snapshot],
        )?;
        Ok(read_mcp_native_link(connection, server_id, app_id)?
            .is_some_and(|row| row.native_snapshot.as_deref() == native_snapshot))
    })
}

/// Removes all application ownership links for one shared MCP server.
pub fn delete_mcp_native_links(
    transaction: &mut Transaction<'_>,
    server_id: &str,
) -> Result<(), SharedStoreError> {
    prepare_mcp_native_link_write(transaction)?;
    execute_mcp_native_link_write(transaction, |connection| {
        connection.execute(
            "DELETE FROM main.mcp_native_links WHERE server_id COLLATE BINARY = ?1",
            [server_id],
        )?;
        let remains = connection.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM main.mcp_native_links
                  WHERE server_id COLLATE BINARY = ?1
             )",
            [server_id],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(!remains)
    })
}

fn mcp_native_link_from_row(row: &Row<'_>) -> Result<McpNativeLinkRow, rusqlite::Error> {
    Ok(McpNativeLinkRow {
        server_id: row.get(0)?,
        app_id: row.get(1)?,
        native_snapshot: row.get(2)?,
    })
}

fn prepare_mcp_native_link_write(transaction: &Transaction<'_>) -> Result<(), SharedStoreError> {
    if transaction.is_autocommit() {
        return Err(SharedStoreError::McpNativeLinkWrite {
            code: None,
            extended_code: None,
            transaction_aborted: true,
        });
    }
    verify_mcp_native_link_schema(transaction)?;
    verify_delete_trigger(transaction)
        .map_err(|error| redact_native_link_store_error(error, transaction.is_autocommit()))
}

fn execute_mcp_native_link_write(
    transaction: &mut Transaction<'_>,
    write: impl FnOnce(&Connection) -> Result<bool, SharedStoreError>,
) -> Result<(), SharedStoreError> {
    let transaction_aborted = transaction.is_autocommit();
    let savepoint = transaction
        .savepoint()
        .map_err(|error| redact_native_link_write_error(error, transaction_aborted))?;
    let result = write(&savepoint);
    match result {
        Ok(true) => savepoint
            .commit()
            .map_err(|error| redact_native_link_write_error(error, transaction.is_autocommit())),
        Ok(false) => {
            savepoint.finish().map_err(|error| {
                redact_native_link_write_error(error, transaction.is_autocommit())
            })?;
            Err(SharedStoreError::InvalidDatabase(
                "MCP native-link write did not reach its requested state".to_owned(),
            ))
        }
        Err(error) => {
            let _ = savepoint.finish();
            Err(redact_native_link_store_error(
                error,
                transaction.is_autocommit(),
            ))
        }
    }
}

fn redact_native_link_store_error(
    error: SharedStoreError,
    transaction_aborted: bool,
) -> SharedStoreError {
    match error {
        SharedStoreError::Database(error) => {
            redact_native_link_write_error(error, transaction_aborted)
        }
        error => error,
    }
}

fn redact_native_link_write_error(
    error: rusqlite::Error,
    transaction_aborted: bool,
) -> SharedStoreError {
    let extended_code = match &error {
        rusqlite::Error::SqliteFailure(error, _) => Some(error.extended_code),
        _ => None,
    };
    SharedStoreError::McpNativeLinkWrite {
        code: error.sqlite_error_code(),
        extended_code,
        transaction_aborted,
    }
}

fn verify_mcp_native_link_schema(connection: &Connection) -> Result<(), SharedStoreError> {
    let table_sql = connection
        .query_row(
            "SELECT sql FROM main.sqlite_schema
             WHERE type = 'table' AND name = 'mcp_native_links'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase("mcp_native_links must be a table".to_owned())
        })?;
    if has_non_abort_conflict_policy(&table_sql) {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_native_links constraints must use ABORT conflict handling".to_owned(),
        ));
    }

    let mut statement = connection.prepare("PRAGMA main.table_xinfo(mcp_native_links)")?;
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
        .collect::<Result<Vec<_>, _>>()?;

    for (name, not_null, primary_key) in [
        ("server_id", true, 1),
        ("app_id", true, 2),
        ("native_snapshot", false, 0),
    ] {
        let actual = columns
            .iter()
            .find(|column| column.name == name)
            .ok_or_else(|| {
                SharedStoreError::InvalidDatabase(format!(
                    "mcp_native_links is missing required column '{name}'"
                ))
            })?;
        if actual.hidden != 0
            || !actual.declared_type.eq_ignore_ascii_case("TEXT")
            || actual.not_null != not_null
            || actual.default.is_some()
            || actual.primary_key != primary_key
        {
            return Err(SharedStoreError::InvalidDatabase(format!(
                "mcp_native_links column '{name}' does not match the shared contract"
            )));
        }
    }

    if columns
        .iter()
        .filter(|column| column.primary_key > 0)
        .count()
        != 2
    {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_native_links primary key must be exactly (server_id, app_id)".to_owned(),
        ));
    }
    verify_binary_primary_key(connection)
}

fn ensure_delete_trigger(connection: &Connection) -> Result<(), SharedStoreError> {
    let trigger_exists = connection.query_row(
        "SELECT EXISTS (
                 SELECT 1 FROM main.sqlite_schema
                  WHERE type = 'trigger' AND name = ?1
             )",
        [MCP_NATIVE_LINK_DELETE_TRIGGER],
        |row| row.get::<_, bool>(0),
    )?;
    if trigger_exists {
        verify_delete_trigger(connection)
    } else {
        connection
            .execute_batch(CREATE_MCP_NATIVE_LINK_DELETE_TRIGGER)
            .map_err(SharedStoreError::from)
    }
}

fn verify_delete_trigger(connection: &Connection) -> Result<(), SharedStoreError> {
    let sql = connection
        .query_row(
            "SELECT sql FROM main.sqlite_schema
             WHERE type = 'trigger' AND name = ?1 AND tbl_name = 'mcp_servers'",
            [MCP_NATIVE_LINK_DELETE_TRIGGER],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(
                "shared MCP native-link cleanup trigger is missing".to_owned(),
            )
        })?;
    if normalize_sql(&sql) != normalize_sql(CREATE_MCP_NATIVE_LINK_DELETE_TRIGGER) {
        return Err(SharedStoreError::InvalidDatabase(
            "shared MCP native-link cleanup trigger does not match its contract".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn verify_binary_primary_key(connection: &Connection) -> Result<(), SharedStoreError> {
    let primary_key_index = connection
        .query_row(
            "SELECT name FROM pragma_index_list('mcp_native_links', 'main')
             WHERE origin = 'pk' AND partial = 0",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(
                "mcp_native_links table has no canonical primary key index".to_owned(),
            )
        })?;
    let mut statement = connection.prepare(
        "SELECT name, \"desc\", coll FROM pragma_index_xinfo(?1, 'main')
         WHERE key = 1 ORDER BY seqno",
    )?;
    let columns = statement
        .query_map([primary_key_index], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        (Some("server_id".to_owned()), 0, "BINARY".to_owned()),
        (Some("app_id".to_owned()), 0, "BINARY".to_owned()),
    ];
    if columns != expected {
        return Err(SharedStoreError::InvalidDatabase(
            "mcp_native_links primary key must use binary server and app ordering".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{begin_immediate_transaction, ensure_mcp_server_schema, SharedDatabase};

    fn initialized_database() -> (tempfile::TempDir, SharedDatabase) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute(
                "INSERT INTO mcp_servers (id, name, server_config) VALUES ('server', 'Server', '{}')",
                [],
            )
            .expect("insert MCP server");
        ensure_mcp_native_link_schema(&mut connection).expect("initialize native links");
        drop(connection);
        (directory, database)
    }

    #[test]
    fn accepts_lite_schema_without_changing_product_version() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "PRAGMA user_version = 31;
                 CREATE TABLE mcp_native_links (
                    server_id TEXT NOT NULL,
                    app_id TEXT NOT NULL,
                    native_snapshot TEXT,
                    PRIMARY KEY (server_id, app_id)
                 );",
            )
            .expect("create Lite schema");

        ensure_mcp_native_link_schema(&mut connection).expect("accept Lite schema");

        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("read product version"),
            31
        );
    }

    #[test]
    fn first_install_claims_enabled_legacy_catalog_rows() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "INSERT INTO mcp_servers (
                    id, name, server_config, enabled_claude, enabled_codex
                 ) VALUES ('enabled', 'Enabled', '{}', 1, 1);
                 INSERT INTO mcp_servers (
                    id, name, server_config, enabled_claude, enabled_codex
                 ) VALUES ('disabled', 'Disabled', '{}', 0, 0);",
            )
            .expect("insert legacy MCP rows");

        ensure_mcp_native_link_schema(&mut connection).expect("initialize native links");

        assert!(read_mcp_native_link(&connection, "enabled", "claude")
            .unwrap()
            .is_some());
        assert!(read_mcp_native_link(&connection, "enabled", "codex")
            .unwrap()
            .is_some());
        assert!(read_mcp_native_link(&connection, "disabled", "claude")
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_existing_link_table_does_not_claim_unowned_rows() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "INSERT INTO mcp_servers (id, name, server_config, enabled_claude)
                 VALUES ('unowned', 'Unowned', '{}', 1);
                 CREATE TABLE mcp_native_links (
                    server_id TEXT NOT NULL,
                    app_id TEXT NOT NULL,
                    native_snapshot TEXT,
                    PRIMARY KEY (server_id, app_id)
                 );",
            )
            .expect("create existing ownership schema");

        ensure_mcp_native_link_schema(&mut connection).expect("validate native links");

        assert!(read_mcp_native_link(&connection, "unowned", "claude")
            .unwrap()
            .is_none());
    }

    #[test]
    fn ignores_same_named_objects_from_separate_sqlite_namespaces() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "CREATE TABLE host_events (value TEXT);
                 CREATE TRIGGER mcp_native_links AFTER INSERT ON host_events BEGIN
                    SELECT NEW.value;
                 END;
                 CREATE TABLE cc_switch_mcp_native_links_after_server_delete (value TEXT);",
            )
            .expect("create same-named host objects");

        ensure_mcp_native_link_schema(&mut connection).expect("initialize native links");

        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema
                     WHERE name IN ('mcp_native_links', ?1)",
                    [MCP_NATIVE_LINK_DELETE_TRIGGER],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count separate namespace objects"),
            4
        );
    }

    #[test]
    fn distinguishes_absent_owned_and_snapshotted_links() {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().expect("connect shared database");
        assert!(read_mcp_native_link(&connection, "server", "claude")
            .expect("read absent link")
            .is_none());

        let mut transaction = begin_immediate_transaction(&mut connection).expect("begin write");
        upsert_mcp_native_link(&mut transaction, "server", "claude", None)
            .expect("record ownership");
        transaction.commit().expect("commit ownership");
        let owned = read_mcp_native_link(&connection, "server", "claude")
            .expect("read ownership")
            .expect("owned link exists");
        assert_eq!(owned.native_snapshot, None);

        let mut transaction = begin_immediate_transaction(&mut connection).expect("begin write");
        upsert_mcp_native_link(
            &mut transaction,
            "server",
            "claude",
            Some("{\"token\":\"secret\"}"),
        )
        .expect("replace snapshot");
        transaction.commit().expect("commit snapshot");
        let snapshotted = read_mcp_native_link(&connection, "server", "claude")
            .expect("read snapshot")
            .expect("snapshotted link exists");
        assert_eq!(
            snapshotted.native_snapshot.as_deref(),
            Some("{\"token\":\"secret\"}")
        );
        assert!(!format!("{snapshotted:?}").contains("secret"));
    }

    #[test]
    fn writes_use_binary_identity_and_preserve_host_columns() {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "ALTER TABLE mcp_native_links ADD COLUMN host_note TEXT DEFAULT 'keep';
                 INSERT INTO mcp_servers (id, name, server_config) VALUES ('Server', 'Upper', '{}');",
            )
            .expect("add host extension");
        let mut transaction = begin_immediate_transaction(&mut connection).expect("begin write");
        upsert_mcp_native_link(&mut transaction, "server", "claude", Some("one"))
            .expect("insert lower link");
        upsert_mcp_native_link(&mut transaction, "Server", "claude", Some("upper"))
            .expect("insert upper link");
        upsert_mcp_native_link(&mut transaction, "server", "claude", Some("two"))
            .expect("update lower link");
        transaction.commit().expect("commit links");

        assert_eq!(
            connection
                .query_row(
                    "SELECT native_snapshot, host_note FROM mcp_native_links
                     WHERE server_id = 'server' AND app_id = 'claude'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read lower link"),
            ("two".to_owned(), "keep".to_owned())
        );
        assert_eq!(
            read_mcp_native_link(&connection, "Server", "claude")
                .expect("read upper link")
                .expect("upper link exists")
                .native_snapshot
                .as_deref(),
            Some("upper")
        );
    }

    #[test]
    fn initialization_removes_only_orphaned_links() {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "INSERT INTO mcp_native_links VALUES ('server', 'claude', NULL);
                 INSERT INTO mcp_native_links VALUES ('missing', 'codex', 'snapshot');",
            )
            .expect("insert native links");

        ensure_mcp_native_link_schema(&mut connection).expect("remove orphaned link");

        assert!(read_mcp_native_link(&connection, "server", "claude")
            .expect("read retained link")
            .is_some());
        assert!(read_mcp_native_link(&connection, "missing", "codex")
            .expect("read orphaned link")
            .is_none());
    }

    #[test]
    fn links_require_a_parent_and_follow_catalog_deletion() {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction = begin_immediate_transaction(&mut connection).expect("begin write");
        let error = upsert_mcp_native_link(&mut transaction, "missing", "claude", None)
            .expect_err("reject orphaned link");
        assert!(error.to_string().contains("existing shared server"));
        upsert_mcp_native_link(&mut transaction, "server", "claude", Some("snapshot"))
            .expect("insert owned link");
        let server = crate::read_mcp_server_row(&transaction, "server")
            .expect("read MCP server")
            .expect("MCP server exists");
        assert_eq!(
            crate::delete_mcp_server(&mut transaction, "server", server.source_fingerprint())
                .expect("delete MCP server"),
            crate::McpServerWriteOutcome::Applied
        );
        assert!(read_mcp_native_link(&transaction, "server", "claude")
            .expect("read cascaded link")
            .is_none());
        transaction.commit().expect("commit delete");
    }

    #[test]
    fn rejects_non_abort_extension_constraints_before_writing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "INSERT INTO mcp_servers (id, name, server_config) VALUES
                    ('one', 'One', '{}'), ('two', 'Two', '{}');
                 CREATE TABLE mcp_native_links (
                    server_id TEXT NOT NULL,
                    app_id TEXT NOT NULL,
                    native_snapshot TEXT UNIQUE ON CONFLICT REPLACE,
                    PRIMARY KEY (server_id, app_id)
                 );",
            )
            .expect("create unsafe extension constraint");

        let error = ensure_mcp_native_link_schema(&mut connection)
            .expect_err("reject replacement constraint");
        assert!(error.to_string().contains("ABORT conflict handling"));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM mcp_native_links", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read unchanged links"),
            0
        );
    }

    #[test]
    fn orphan_cleanup_errors_are_redacted_and_rolled_back() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "CREATE TABLE mcp_native_links (
                    server_id TEXT NOT NULL,
                    app_id TEXT NOT NULL,
                    native_snapshot TEXT,
                    PRIMARY KEY (server_id, app_id)
                 );
                 INSERT INTO mcp_native_links VALUES ('missing', 'claude', 'secret snapshot');
                 CREATE TRIGGER fail_native_cleanup BEFORE DELETE ON mcp_native_links BEGIN
                    SELECT RAISE(FAIL, 'private native snapshot detail');
                 END;",
            )
            .expect("create failing cleanup trigger");

        let error = ensure_mcp_native_link_schema(&mut connection)
            .expect_err("surface redacted cleanup failure");
        assert!(matches!(error, SharedStoreError::McpNativeLinkWrite { .. }));
        assert_eq!(error.to_string(), "shared MCP native-link write failed");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM mcp_native_links", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read rolled-back orphan"),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name = ?1",
                    [MCP_NATIVE_LINK_DELETE_TRIGGER],
                    |row| row.get::<_, i64>(0),
                )
                .expect("verify cleanup trigger rollback"),
            0
        );
    }

    #[test]
    fn rejects_incompatible_primary_key_without_rebuilding_table() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        let mut connection = database.connect().expect("connect shared database");
        ensure_mcp_server_schema(&mut connection).expect("initialize MCP catalog");
        connection
            .execute_batch(
                "CREATE TABLE mcp_native_links (
                    server_id TEXT NOT NULL COLLATE NOCASE,
                    app_id TEXT NOT NULL,
                    native_snapshot TEXT,
                    PRIMARY KEY (server_id, app_id)
                 );",
            )
            .expect("create incompatible native links");

        let error = ensure_mcp_native_link_schema(&mut connection)
            .expect_err("reject incompatible primary key");
        assert!(error.to_string().contains("binary server and app ordering"));
    }

    #[test]
    fn delete_removes_every_app_link_for_only_one_server() {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "INSERT INTO mcp_servers (id, name, server_config) VALUES ('other', 'Other', '{}');
                 INSERT INTO mcp_native_links VALUES ('server', 'claude', NULL);
                 INSERT INTO mcp_native_links VALUES ('server', 'codex', NULL);
                 INSERT INTO mcp_native_links VALUES ('other', 'claude', NULL);",
            )
            .expect("insert links");
        let mut transaction = begin_immediate_transaction(&mut connection).expect("begin write");

        delete_mcp_native_links(&mut transaction, "server").expect("delete server links");
        transaction.commit().expect("commit delete");

        assert!(read_mcp_native_link(&connection, "server", "claude")
            .expect("read deleted link")
            .is_none());
        assert!(read_mcp_native_link(&connection, "other", "claude")
            .expect("read retained link")
            .is_some());
    }
}
