use super::*;
use crate::{read_provider_row, ProviderWriteOutcome};
use std::time::Duration;

fn seed(connection: &Connection) {
    connection
        .execute_batch(crate::CREATE_PROVIDERS_TABLE)
        .unwrap();
    connection
        .execute_batch(
            "INSERT INTO providers(id, app_type, name, settings_config, is_current)
         VALUES ('old', 'future-agent', 'Old', '{\"hooks\":{\"keep\":true}}', 1),
                ('new', 'future-agent', 'New', '{}', 0),
                ('peer', 'other-app', 'Peer', 'opaque', 0);
         ALTER TABLE providers ADD COLUMN future_host BLOB DEFAULT X'010203';
         ALTER TABLE mcp_native_links ADD COLUMN future_host TEXT DEFAULT 'keep';
         CREATE TABLE host_state(value TEXT);
         PRAGMA user_version=72;",
        )
        .unwrap();
}

fn select(guard: &mut McpTransactionGuard<'_>, id: &str, selected: bool) {
    let row = guard.read_provider(id, "future-agent").unwrap().unwrap();
    assert_eq!(
        guard
            .set_provider_current_if_unchanged(
                id,
                "future-agent",
                row.source_fingerprint(),
                selected,
            )
            .unwrap(),
        ProviderWriteOutcome::Applied
    );
}

fn reject_commit(guard: McpTransactionGuard<'_>) -> (McpTransactionGuard<'_>, SharedStoreError) {
    match guard.commit_preserving_on_error() {
        Ok(()) => panic!("unexpected commit"),
        Err(failure) => failure,
    }
}

#[test]
fn requested_provider_value_and_unowned_generated_columns_are_verified() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    for settings in [false, true] {
        for generated in [false, true] {
            let (_directory, database) = initialized_database();
            let mut connection = database.connect().unwrap();
            seed(&connection);
            let column = if settings {
                "settings_config"
            } else {
                "is_current"
            };
            if generated {
                connection.execute_batch(&format!(
                    "ALTER TABLE providers ADD COLUMN \"future\"\"derived\" GENERATED ALWAYS AS ({column}) VIRTUAL;"
                )).unwrap();
            } else {
                connection.authorizer(Some(move |context: AuthContext<'_>| match context.action {
                    AuthAction::Update {
                        table_name: "providers",
                        column_name,
                    } if column_name == column => Authorization::Ignore,
                    _ => Authorization::Allow,
                }));
            }
            let before = provider::read_fingerprints(&connection).unwrap();
            let mut guard = McpTransactionGuard::from_provider_transaction(
                begin_immediate_transaction(&mut connection).unwrap(),
            )
            .unwrap();
            let row = guard.read_provider("new", "future-agent").unwrap().unwrap();
            guard
                .upsert_native_link("server", "gemini", Some("must roll back"))
                .unwrap();
            let result = if settings {
                guard.update_provider_settings_config_if_unchanged(
                    "new",
                    "future-agent",
                    row.source_fingerprint(),
                    "changed",
                )
            } else {
                guard.set_provider_current_if_unchanged(
                    "new",
                    "future-agent",
                    row.source_fingerprint(),
                    true,
                )
            };
            assert!(
                result.is_err() || result.unwrap() == ProviderWriteOutcome::NotApplied,
                "settings={settings}, generated={generated}"
            );
            let (guard, _) = reject_commit(guard);
            guard.rollback().unwrap();
            connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
            assert_eq!(provider::read_fingerprints(&connection).unwrap(), before);
            assert!(read_mcp_native_link(&connection, "server", "gemini")
                .unwrap()
                .is_none());
        }
    }
}

#[test]
fn composite_success_preserves_extensions_and_allows_later_provider_writes() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().unwrap();
    seed(&connection);
    let peer = read_provider_row(&connection, "peer", "other-app").unwrap();
    let transaction = begin_immediate_transaction(&mut connection).unwrap();
    transaction
        .execute("INSERT INTO host_state VALUES ('host-owned')", [])
        .unwrap();
    let mut guard = McpTransactionGuard::from_provider_transaction(transaction).unwrap();
    assert_eq!(guard.read_providers(Some("future-agent")).unwrap().len(), 2);
    select(&mut guard, "old", false);
    select(&mut guard, "new", true);
    // Registry capabilities do not restrict the host's raw provider App identity.
    for descriptor in builtin_app_registry().descriptors() {
        let Some(contract) = descriptor.mcp_contract() else {
            continue;
        };
        let row = guard.read_server("server").unwrap().unwrap();
        guard
            .set_server_selection(
                "server",
                row.source_fingerprint(),
                contract.catalog_column(),
                true,
            )
            .unwrap();
        guard
            .upsert_native_link("server", descriptor.id(), Some("opaque native snapshot"))
            .unwrap();
    }
    let row = guard.read_provider("new", "future-agent").unwrap().unwrap();
    let settings = "{\"hooks\":{\"keep\":true},\"auth\":{\"opaque\":123}}";
    assert_eq!(
        guard
            .update_provider_settings_config_if_unchanged(
                "new",
                "future-agent",
                row.source_fingerprint(),
                settings,
            )
            .unwrap(),
        ProviderWriteOutcome::Applied
    );
    assert!(guard.commit_preserving_on_error().is_ok());
    let row = read_provider_row(&connection, "new", "future-agent")
        .unwrap()
        .unwrap();
    assert_eq!(row.settings_config, settings);
    assert_eq!(row.is_current, 1);
    assert_eq!(
        read_provider_row(&connection, "old", "future-agent")
            .unwrap()
            .unwrap()
            .is_current,
        0
    );
    assert_eq!(
        read_provider_row(&connection, "peer", "other-app").unwrap(),
        peer
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT future_host FROM providers WHERE id='new'",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        [1, 2, 3]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM mcp_native_links WHERE future_host != 'keep'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT value FROM host_state", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "host-owned"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        72
    );
}

#[test]
fn provider_failures_poison_the_whole_composite_without_committing_mcp() {
    for failure in [
        "stale",
        "ignore",
        "abort",
        "rollback",
        "mcp-side-write",
        "peer-side-write",
    ] {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().unwrap();
        seed(&connection);
        let before = provider::read_fingerprints(&connection).unwrap();
        let trigger = match failure {
            "ignore" => "SELECT RAISE(IGNORE);",
            "abort" => "SELECT RAISE(ABORT, 'fixture');",
            "rollback" => "SELECT RAISE(ROLLBACK, 'fixture');",
            "mcp-side-write" => "UPDATE mcp_servers SET name='side-write';",
            "peer-side-write" => "UPDATE providers SET future_host=X'0405' WHERE id='peer';",
            _ => "SELECT 1;",
        };
        connection.execute_batch(&format!(
            "CREATE TRIGGER fixture BEFORE UPDATE ON providers WHEN OLD.id='new' BEGIN {trigger} END;"
        )).unwrap();
        let transaction = begin_immediate_transaction(&mut connection).unwrap();
        let mut guard = McpTransactionGuard::from_provider_transaction(transaction).unwrap();
        guard
            .upsert_native_link("server", "claude", Some("owned"))
            .unwrap();
        let row = guard.read_provider("new", "future-agent").unwrap().unwrap();
        let fingerprint = if failure == "stale" {
            &[0; 32]
        } else {
            row.source_fingerprint()
        };
        let result =
            guard.set_provider_current_if_unchanged("new", "future-agent", fingerprint, true);
        assert!(
            result.is_err() || result.unwrap() == ProviderWriteOutcome::NotApplied,
            "{failure}"
        );
        assert!(guard.upsert_native_link("server", "gemini", None).is_err());
        assert!(guard.read_providers(None).is_err());
        let (guard, error) = reject_commit(guard);
        assert!(matches!(error, SharedStoreError::McpTransactionConflict));
        guard.rollback().unwrap();
        assert_eq!(provider::read_fingerprints(&connection).unwrap(), before);
        assert!(read_mcp_native_link(&connection, "server", "claude")
            .unwrap()
            .is_none());
        assert_eq!(
            read_mcp_server_row(&connection, "server")
                .unwrap()
                .unwrap()
                .name,
            "Server"
        );
    }
}

#[test]
fn mcp_side_effects_on_providers_are_checked_before_later_work_and_commit() {
    for later_write in [false, true] {
        let (_directory, database) = initialized_database();
        let mut connection = database.connect().unwrap();
        seed(&connection);
        let before = provider::read_fingerprints(&connection).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fixture AFTER UPDATE ON mcp_servers BEGIN
             UPDATE providers SET future_host=X'09' WHERE id='peer'; END;",
            )
            .unwrap();
        let mut guard = McpTransactionGuard::from_provider_transaction(
            begin_immediate_transaction(&mut connection).unwrap(),
        )
        .unwrap();
        select(&mut guard, "old", false);
        let row = guard.read_server("server").unwrap().unwrap();
        // The MCP primitive owns its target row; the composite final-state check
        // must also catch its trigger's otherwise invisible provider modification.
        guard
            .set_server_selection(
                "server",
                row.source_fingerprint(),
                mcp_column("claude"),
                true,
            )
            .unwrap();
        if later_write {
            assert!(guard.read_provider("new", "future-agent").is_err());
        }
        let (guard, error) = reject_commit(guard);
        assert!(matches!(error, SharedStoreError::McpTransactionConflict));
        guard.rollback().unwrap();
        assert_eq!(provider::read_fingerprints(&connection).unwrap(), before);
        assert_eq!(
            read_mcp_server_row(&connection, "server")
                .unwrap()
                .unwrap()
                .enabled_claude,
            0
        );
    }
}

#[test]
fn construction_and_read_errors_cannot_leave_a_committable_partial_guard() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().unwrap();
    seed(&connection);
    let mut transaction = begin_immediate_transaction(&mut connection).unwrap();
    transaction.set_drop_behavior(DropBehavior::Commit);
    transaction
        .execute("INSERT INTO host_state VALUES ('must-rollback')", [])
        .unwrap();
    transaction.execute_batch("DROP TABLE providers").unwrap();
    assert!(McpTransactionGuard::from_provider_transaction(transaction).is_err());
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM host_state", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let transaction = begin_immediate_transaction(&mut connection).unwrap();
    transaction.execute_batch("ROLLBACK").unwrap();
    assert!(McpTransactionGuard::from_provider_transaction(transaction).is_err());

    // Opaque unrelated provider values need not be decoded to guard their bytes.
    connection
        .execute_batch("UPDATE providers SET is_current='malformed' WHERE id='peer'")
        .unwrap();
    let mut guard = McpTransactionGuard::from_provider_transaction(
        begin_immediate_transaction(&mut connection).unwrap(),
    )
    .unwrap();
    select(&mut guard, "new", true);
    assert!(guard.read_providers(None).is_err());
    let (guard, _) = reject_commit(guard);
    guard.rollback().unwrap();
    assert_eq!(
        read_provider_row(&connection, "new", "future-agent")
            .unwrap()
            .unwrap()
            .is_current,
        0
    );

    let mut guard = McpTransactionGuard::begin(&mut connection).unwrap();
    assert!(guard.read_provider("new", "future-agent").is_err());
    assert!(guard.commit().is_err());
}

/// Synthetic third consumer: Core owns conditional native receipts; this host
/// owns its lock and retains all receipts until the Store commit decision.
#[test]
fn composite_consumer_retains_native_recovery_and_database_lock_until_final_decision() {
    use cc_switch_core::fs::{SharedLiveConfigLock, SharedLiveConfigLockError};
    use cc_switch_core::{
        execute_mcp_write_with_content_limit, ContentExpectation, McpConfigTarget,
    };

    for failure in ["success", "commit", "host", "external"] {
        let (directory, database) = initialized_database();
        let mut connection = database.connect().unwrap();
        seed(&connection);
        if failure == "commit" {
            connection
                .execute_batch(
                    "CREATE UNIQUE INDEX fixture_current ON providers(id, app_type, is_current);
                 CREATE TABLE fixture_deferred(id TEXT, app TEXT, selected INTEGER,
                   FOREIGN KEY(id,app,selected) REFERENCES providers(id,app_type,is_current)
                   DEFERRABLE INITIALLY DEFERRED);
                 INSERT INTO fixture_deferred VALUES ('old','future-agent',1);",
                )
                .unwrap();
        }
        let before = provider::read_fingerprints(&connection).unwrap();
        let peer = database.connect().unwrap();
        peer.busy_timeout(Duration::ZERO).unwrap();
        let lock_path = directory.path().join("live.lock");
        let native_lock;
        let mut guard = McpTransactionGuard::from_provider_transaction(
            begin_immediate_transaction(&mut connection).unwrap(),
        )
        .unwrap();
        native_lock = SharedLiveConfigLock::try_acquire(&lock_path).unwrap();
        select(&mut guard, "old", false);
        select(&mut guard, "new", true);
        let mut native = Native(b"{\"extension\":true}".to_vec());
        let original = native.0.clone();
        let mut receipts = Vec::new();
        for contents in [
            "{\"extension\":true,\"provider\":1}",
            "{\"extension\":true,\"provider\":1,\"mcp\":2}",
        ] {
            let expected = ContentExpectation::for_contents(Some(&native.0));
            receipts.push(
                execute_mcp_write_with_content_limit(
                    McpConfigTarget::Gemini,
                    &expected,
                    contents,
                    &mut native,
                    1024,
                )
                .unwrap(),
            );
        }
        guard
            .upsert_native_link("server", "gemini", Some("retained"))
            .unwrap();
        let row = guard.read_provider("new", "future-agent").unwrap().unwrap();
        assert_eq!(
            guard
                .update_provider_settings_config_if_unchanged(
                    "new",
                    "future-agent",
                    row.source_fingerprint(),
                    "{\"snapshot\":true}",
                )
                .unwrap(),
            ProviderWriteOutcome::Applied
        );
        assert_ne!(native.0, original);
        if failure == "success" {
            assert!(guard.commit_preserving_on_error().is_ok());
            assert_eq!(
                read_provider_row(&peer, "new", "future-agent")
                    .unwrap()
                    .unwrap()
                    .is_current,
                1
            );
            assert!(read_mcp_native_link(&peer, "server", "gemini")
                .unwrap()
                .is_some());
        } else {
            let guard = if failure == "commit" {
                let (guard, error) = reject_commit(guard);
                assert!(
                    matches!(error, SharedStoreError::Database(rusqlite::Error::SqliteFailure(error, _))
                    if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
                );
                guard
            } else {
                guard
            };
            assert!(
                matches!(peer.execute_batch("BEGIN IMMEDIATE"), Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy)
            );
            assert!(matches!(
                SharedLiveConfigLock::try_acquire(&lock_path),
                Err(SharedLiveConfigLockError::Unavailable)
            ));
            if failure == "external" {
                native.0 = b"{\"external\":true}".to_vec();
            }
            for receipt in receipts.into_iter().rev() {
                assert_eq!(
                    receipt.rollback(&mut native).is_err(),
                    failure == "external"
                );
            }
            assert_eq!(
                native.0,
                if failure == "external" {
                    b"{\"external\":true}".to_vec()
                } else {
                    original
                }
            );
            guard.rollback().unwrap();
            assert_eq!(provider::read_fingerprints(&connection).unwrap(), before);
            assert!(read_mcp_native_link(&connection, "server", "gemini")
                .unwrap()
                .is_none());
        }
        drop(native_lock);
        peer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert!(SharedLiveConfigLock::try_acquire(&lock_path).is_ok());
    }
}

struct Native(Vec<u8>);

impl cc_switch_core::OperationHost<cc_switch_core::McpConfigTarget> for Native {
    type Resource = ();
    type Error = std::convert::Infallible;

    fn resolve(&mut self, _: cc_switch_core::McpConfigTarget) -> Result<(), Self::Error> {
        Ok(())
    }

    fn read(
        &mut self,
        _: &(),
        maximum: usize,
    ) -> Result<cc_switch_core::OperationRead, Self::Error> {
        Ok(if self.0.len() > maximum {
            cc_switch_core::OperationRead::TooLarge
        } else {
            cc_switch_core::OperationRead::Contents(self.0.clone())
        })
    }

    fn compare_exchange(
        &mut self,
        _: &(),
        expected: Option<&[u8]>,
        replacement: Option<&[u8]>,
    ) -> Result<cc_switch_core::CompareExchangeOutcome, Self::Error> {
        if expected != Some(self.0.as_slice()) {
            return Ok(cc_switch_core::CompareExchangeOutcome::Conflict);
        }
        self.0 = replacement
            .expect("fixture always restores existing documents")
            .to_vec();
        Ok(cc_switch_core::CompareExchangeOutcome::Applied)
    }
}
