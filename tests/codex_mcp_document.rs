use cc_switch_core::{
    codex::McpDocument, project_mcp_server, AppType, McpServerProjection,
    MAX_OPERATION_CONTENT_BYTES,
};
use serde_json::json;

#[test]
fn native_edits_keep_table_style_siblings_and_unrelated_provider_data() {
    let source = "model = 'keep' # comment\nmcp_servers = { old = { command = 'old' } }\n\n[model_providers.private]\ntoken = 'synthetic-secret'\n\n[mcp.servers.legacy]\ncommand = 'legacy'\n";
    let mut document = McpDocument::parse(source).unwrap();
    assert!(!document
        .upsert_native_server("added.id", "command = 'node'\n[oauth]\nscopes = ['read']\n")
        .unwrap());
    let text = document.render();
    assert!(text.starts_with("model = 'keep' # comment\nmcp_servers = {"));
    assert!(text.contains("[model_providers.private]\ntoken = 'synthetic-secret'"));
    assert!(text.contains("[mcp.servers.legacy]"));
    assert!(document.remove_server("old").removed_official);
    assert!(document.remove_server("legacy").removed_legacy);
    let parsed: toml_edit::DocumentMut = document.render().parse().unwrap();
    assert_eq!(
        parsed["mcp_servers"]["added.id"]["oauth"]["scopes"]
            .as_array()
            .unwrap()
            .get(0)
            .unwrap()
            .as_str(),
        Some("read")
    );
    // Empty implicit parents are omitted by the native serializer.
    assert!(parsed.get("mcp").is_none());
    assert_eq!(format!("{document:?}"), "McpDocument(<redacted>)");
}

#[test]
fn native_repair_and_legacy_cleanup_are_explicit_and_do_not_change_strict_defaults() {
    for source in [
        "mcp_servers = 42\n",
        "mcp_servers = []\n",
        "mcp_servers = 'bad'\n",
    ] {
        let mut document = McpDocument::parse(source).unwrap();
        let removed = document.remove_server("id");
        assert!(removed.malformed_official_collection);
        assert!(!removed.removed_official);
        assert_eq!(document.render(), source);
        assert!(project_mcp_server(
            &AppType::Codex,
            Some(source.as_bytes()),
            "id",
            McpServerProjection::Remove
        )
        .is_err());
        assert!(document
            .upsert_native_server("id", "future = { keep = [1, true] }\n")
            .unwrap());
    }
    let source =
        "[mcp]\nkeep = true\n[mcp.servers.a]\ncommand = 'a'\n[mcp.servers.b]\ncommand = 'b'\n";
    let mut native = McpDocument::parse(source).unwrap();
    assert!(!native
        .upsert_native_server("a", "command = 'updated'\n")
        .unwrap());
    assert!(native.render().contains("[mcp.servers.b]"));
    assert!(native.clear_legacy_servers());
    assert!(!native.clear_legacy_servers());
    assert!(native.render().contains("keep = true"));

    let strict = project_mcp_server(
        &AppType::Codex,
        Some(source.as_bytes()),
        "a",
        McpServerProjection::Enable(&json!({"command":"updated"})),
    )
    .unwrap()
    .unwrap();
    let strict: toml_edit::DocumentMut = strict.parse().unwrap();
    assert!(strict["mcp"]["servers"]
        .as_table_like()
        .unwrap()
        .contains_key("b"));
    assert!(!strict["mcp"]["servers"]
        .as_table_like()
        .unwrap()
        .contains_key("a"));
}

#[test]
fn native_replacement_and_parse_failures_have_defined_mutation_boundaries() {
    let source = "# retained\nmodel = 'keep'\n[mcp_servers.old]\ncommand = 'old'\n";
    let mut document = McpDocument::parse(source).unwrap();
    assert!(document.upsert_native_server("bad", "command = [").is_err());
    assert_eq!(document.render(), source);
    assert!(document
        .replace_native_servers([("first", "command = 'ok'"), ("bad", "command = [")])
        .is_err());
    assert_eq!(document.render(), source);
    document
        .replace_native_servers([
            ("z", "value = 1979-05-27T07:32:00Z\n"),
            ("a", "value = 9223372036854775807\n"),
            ("z", "value = 'last'\n"),
        ])
        .unwrap();
    let text = document.render();
    assert!(text.starts_with("# retained\nmodel = 'keep'\n"));
    let parsed: toml_edit::DocumentMut = text.parse().unwrap();
    assert_eq!(parsed["mcp_servers"]["z"]["value"].as_str(), Some("last"));
    assert_eq!(
        parsed["mcp_servers"]["a"]["value"].as_integer(),
        Some(i64::MAX)
    );
    document.replace_native_servers([]).unwrap();
    assert!(document.render().contains("[mcp_servers]"));
    assert!(document.clear_servers());
    assert!(!document.clear_servers());
    assert_eq!(document.render(), "# retained\nmodel = 'keep'\n");
}

#[test]
fn native_parsing_has_no_implicit_size_or_blank_policy() {
    assert!(McpDocument::parse("\u{a0}").is_err());
    let error = McpDocument::parse("token = 'synthetic-secret'\nbad = [").unwrap_err();
    assert_eq!(format!("{error:?}"), "McpDocumentParseError(<redacted>)");
    let expected = "token = 'synthetic-secret'\nbad = ["
        .parse::<toml_edit::DocumentMut>()
        .unwrap_err();
    assert_eq!(error.to_string(), expected.to_string());
    let source = format!(
        "# {}\nmodel = 'keep'\n",
        "x".repeat(MAX_OPERATION_CONTENT_BYTES)
    );
    let document = McpDocument::parse(&source).unwrap();
    assert_eq!(document.render(), source);
}
