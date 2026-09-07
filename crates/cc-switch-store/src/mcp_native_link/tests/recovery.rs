use super::*;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Failure {
    Commit,
    Verification,
    Poisoned,
    Aborted,
}

fn failed_commit(guard: McpTransactionGuard<'_>) -> (McpTransactionGuard<'_>, SharedStoreError) {
    match guard.commit_preserving_on_error() {
        Ok(()) => panic!("fixture must reject commit"),
        Err(failure) => failure,
    }
}

#[test]
fn recoverable_commit_retains_the_guard_and_rejects_further_writes() {
    for failure in [
        Failure::Commit,
        Failure::Verification,
        Failure::Poisoned,
        Failure::Aborted,
    ] {
        for explicit_rollback in [false, true] {
            let (_directory, database) = initialized_database();
            let mut connection = database.connect().unwrap();
            let trigger = match failure {
                Failure::Commit => "CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY);
                    CREATE TABLE fixture_child(id INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
                    CREATE TRIGGER fixture_reject AFTER UPDATE ON mcp_servers BEGIN
                        INSERT INTO fixture_child VALUES (1); END;",
                Failure::Verification => "CREATE TRIGGER fixture_reject AFTER UPDATE ON mcp_servers BEGIN
                    UPDATE mcp_native_links SET native_snapshot = 'unexpected' WHERE server_id = NEW.id; END;",
                Failure::Poisoned => "CREATE TRIGGER fixture_reject BEFORE UPDATE ON mcp_servers BEGIN
                    SELECT RAISE(IGNORE); END;",
                Failure::Aborted => "CREATE TRIGGER fixture_reject BEFORE UPDATE ON mcp_servers BEGIN
                    SELECT RAISE(ROLLBACK, 'fixture abort'); END;",
            };
            connection.execute_batch(trigger).unwrap();
            let mut guard = McpTransactionGuard::begin(&mut connection).unwrap();
            let before = guard.read_server("server").unwrap().unwrap();
            guard
                .upsert_native_link("server", "claude", Some("owned"))
                .unwrap();
            let write = guard.set_server_selection(
                "server",
                before.source_fingerprint(),
                mcp_column("claude"),
                true,
            );
            assert_eq!(
                write.is_ok(),
                matches!(failure, Failure::Commit | Failure::Verification)
            );
            let (mut guard, error) = failed_commit(guard);
            if matches!(failure, Failure::Commit) {
                assert!(
                    matches!(error, SharedStoreError::Database(rusqlite::Error::SqliteFailure(error, _))
                    if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
                );
            } else {
                assert!(matches!(error, SharedStoreError::McpTransactionConflict));
            }
            assert!(matches!(
                guard.upsert_native_link("server", "gemini", None),
                Err(SharedStoreError::McpTransactionConflict)
            ));
            let (guard, error) = failed_commit(guard);
            assert!(matches!(error, SharedStoreError::McpTransactionConflict));
            let peer = database.connect().unwrap();
            peer.busy_timeout(Duration::ZERO).unwrap();
            let contender = peer.execute_batch("BEGIN IMMEDIATE");
            if matches!(failure, Failure::Aborted) {
                contender.unwrap();
                peer.execute_batch("ROLLBACK").unwrap();
            } else {
                assert!(
                    matches!(contender, Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy),
                    "{failure:?}"
                );
            }
            if explicit_rollback {
                guard.rollback().unwrap();
            } else {
                drop(guard);
            }
            peer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            assert_eq!(
                *read_mcp_server_row(&connection, "server")
                    .unwrap()
                    .unwrap()
                    .source_fingerprint(),
                *before.source_fingerprint()
            );
            assert!(read_mcp_native_link(&connection, "server", "claude")
                .unwrap()
                .is_none());
        }
    }
}

#[test]
fn recoverable_commit_publishes_coherent_state_and_preserves_host_fields() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().unwrap();
    connection.execute_batch(
        "ALTER TABLE mcp_servers ADD COLUMN future_host_value BLOB DEFAULT X'010203';
         ALTER TABLE mcp_native_links ADD COLUMN future_host_value TEXT DEFAULT 'keep';
         INSERT INTO mcp_native_links VALUES ('future-server', 'future-app', 'opaque', 'untouched');"
    ).unwrap();
    let mut guard = McpTransactionGuard::begin(&mut connection).unwrap();
    let before = guard.read_server("server").unwrap().unwrap();
    guard
        .upsert_native_link("server", "claude", Some("owned"))
        .unwrap();
    guard
        .set_server_selection(
            "server",
            before.source_fingerprint(),
            mcp_column("claude"),
            true,
        )
        .unwrap();
    assert!(guard.commit_preserving_on_error().is_ok());
    assert!(connection.is_autocommit());
    assert_eq!(
        read_mcp_server_row(&connection, "server")
            .unwrap()
            .unwrap()
            .enabled_claude,
        1
    );
    assert_eq!(
        read_mcp_native_link(&connection, "server", "claude")
            .unwrap()
            .unwrap()
            .native_snapshot
            .as_deref(),
        Some("owned")
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT future_host_value FROM mcp_servers WHERE id = 'server'",
                [],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT future_host_value FROM mcp_native_links WHERE server_id = 'future-server'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "untouched"
    );
    let peer = database.connect().unwrap();
    peer.busy_timeout(Duration::ZERO).unwrap();
    peer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}
