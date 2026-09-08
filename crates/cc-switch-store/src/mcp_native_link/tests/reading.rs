use super::*;

#[test]
fn catalog_read_uses_fresh_rows_and_includes_guarded_writes() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().expect("connect");
    assert_eq!(
        read_mcp_server_rows(&connection)
            .expect("initial rows")
            .len(),
        1
    );
    let peer = database.connect().expect("peer connection");
    peer.execute_batch(
        "ALTER TABLE mcp_servers ADD COLUMN host_note TEXT DEFAULT 'default';
         INSERT INTO mcp_servers (id, name, server_config, host_note)
         VALUES ('b', 'Peer', '{}', 'host-b'), ('a', 'Peer', '{}', 'host-a');",
    )
    .expect("peer commits before guard");
    let before = read_mcp_server_rows(&connection).expect("committed rows");
    let mut guard = McpTransactionGuard::begin(&mut connection).expect("guard");
    let rows = guard.read_servers().expect("guarded catalog");
    assert_eq!(rows, before);
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["a", "b", "server"]
    );
    for row in &rows {
        assert_eq!(
            guard.read_server(&row.id).expect("single row").as_ref(),
            Some(row)
        );
    }
    guard
        .set_server_selection(
            "a",
            rows[0].source_fingerprint(),
            mcp_column("claude"),
            true,
        )
        .expect("enable one App");
    let updated = guard.read_servers().expect("read own write");
    assert_eq!(updated[0].enabled_claude, 1);
    assert_ne!(
        updated[0].source_fingerprint(),
        rows[0].source_fingerprint()
    );
    assert_eq!(updated[1..], rows[1..]);
    guard.commit().expect("verify and commit");
    assert_eq!(
        read_mcp_server_rows(&connection).expect("stored rows"),
        updated
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT host_note FROM mcp_servers WHERE id='a'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("host field"),
        "host-a"
    );
}

#[test]
fn catalog_read_returns_empty_after_guarded_delete() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().expect("connect");
    let mut guard = McpTransactionGuard::begin(&mut connection).expect("guard");
    let rows = guard.read_servers().expect("catalog");
    guard
        .delete_server("server", rows[0].source_fingerprint())
        .expect("delete");
    assert!(guard.read_servers().expect("empty catalog").is_empty());
    guard.commit().expect("commit");
    assert!(read_mcp_server_rows(&connection)
        .expect("stored catalog")
        .is_empty());
}

#[test]
fn ignored_catalog_read_error_poison_rolls_back_previous_writes() {
    let (_directory, database) = initialized_database();
    let mut connection = database.connect().expect("connect");
    let before = read_mcp_server_rows(&connection).expect("initial catalog");
    let mut guard = McpTransactionGuard::begin(&mut connection).expect("guard");
    guard
        .upsert_native_link("server", "claude", None)
        .expect("write link");
    // Test-only fault injection. Repair the exact bytes afterward so commit
    // cannot reject merely because of a remaining catalog difference.
    guard
        .transaction
        .execute("UPDATE mcp_servers SET tags=X'00' WHERE id='server'", [])
        .expect("inject read failure");
    assert!(guard.read_servers().is_err());
    guard
        .transaction
        .execute("UPDATE mcp_servers SET tags='[]' WHERE id='server'", [])
        .expect("repair fixture");
    assert!(matches!(
        guard.commit(),
        Err(SharedStoreError::McpTransactionConflict)
    ));
    assert_eq!(read_mcp_server_rows(&connection).expect("catalog"), before);
    assert!(read_mcp_native_link(&connection, "server", "claude")
        .expect("rolled-back link")
        .is_none());
}
