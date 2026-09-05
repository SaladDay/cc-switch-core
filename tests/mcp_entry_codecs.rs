use cc_switch_core::{
    builtin_app_adapters, import_mcp_servers, mcp_servers_equivalent, project_mcp_server,
    validate_mcp_server, McpConfigTarget, McpServerProjection,
};
use serde_json::{json, Value};

#[test]
fn every_registered_mcp_codec_agrees_with_document_projection() {
    for adapter in builtin_app_adapters() {
        let Some(target) = adapter.mcp_config_target() else {
            continue;
        };
        let app = adapter.descriptor().app();
        for server in [
            json!({"type":"stdio", "command":"npx", "args":["server"], "env":{"KEY":"value"}}),
            json!({"type":"http", "url":"https://example.com/mcp", "headers":{"X-Test":"value"}}),
            json!({"type":"sse", "url":"https://example.com/sse"}),
        ] {
            let native = target.encode_server(&server).expect("encode entry");
            let decoded = target.decode_server(&native).expect("decode entry");
            validate_mcp_server("test", &decoded).expect("valid decoded connection");
            assert!(
                mcp_servers_equivalent(app, &server, &decoded),
                "{app:?}: {native}"
            );

            let document =
                project_mcp_server(app, None, "test", McpServerProjection::Enable(&server))
                    .expect("project document")
                    .expect("new document");
            let root: Value = match target {
                McpConfigTarget::Codex | McpConfigTarget::GrokBuild => {
                    // Document import also observes native enablement separately.
                    let imports =
                        import_mcp_servers(app, Some(document.as_bytes())).expect("import");
                    assert_eq!(imports.len(), 1);
                    assert!(mcp_servers_equivalent(app, &decoded, &imports[0].server));
                    continue;
                }
                McpConfigTarget::Hermes => serde_yaml::from_str(&document).expect("YAML"),
                _ => serde_json::from_str(&document).expect("JSON"),
            };
            let section = match target {
                McpConfigTarget::OpenCode => "mcp",
                McpConfigTarget::Hermes => "mcp_servers",
                _ => "mcpServers",
            };
            assert_eq!(root[section]["test"], native, "{app:?}");
        }
    }
}

#[test]
fn structural_codecs_do_not_replace_connection_validation() {
    for target in [McpConfigTarget::OpenCode, McpConfigTarget::Hermes] {
        let server = json!({"type":"http", "url":"", "headers":{"X-Test":42}});
        assert!(validate_mcp_server("test", &server).is_err());
        let native = target
            .encode_server(&server)
            .expect("structural conversion");
        assert_eq!(native["url"], "");
        assert_eq!(native["headers"]["X-Test"], 42);
        assert!(target
            .encode_server(&json!({"type":"unsupported"}))
            .is_err());
    }
    for adapter in builtin_app_adapters() {
        if let Some(target) = adapter.mcp_config_target() {
            for malformed in [Value::Null, json!([]), json!(false), json!(42)] {
                assert!(target.encode_server(&malformed).is_err());
                assert!(target.decode_server(&malformed).is_err());
            }
        }
    }
}

#[test]
fn codecs_keep_native_extensions_for_the_host_to_select() {
    for target in [McpConfigTarget::OpenCode, McpConfigTarget::Hermes] {
        let server = json!({"command":"npx", "extension":{"setting":true}});
        let native = target.encode_server(&server).expect("encode");
        assert_eq!(native["extension"], server["extension"]);
        let decoded = target.decode_server(&native).expect("decode");
        assert_eq!(decoded["extension"], server["extension"]);
    }
    let decoded = McpConfigTarget::Codex
        .decode_server(&json!({
            "url":"https://example.com/mcp", "http_headers":{"X-Test":"value"}, "enabled":false,
        }))
        .expect("decode Codex");
    assert_eq!(decoded["type"], "http");
    assert_eq!(decoded["headers"], json!({"X-Test":"value"}));
    assert_eq!(decoded["enabled"], false);
    assert!(decoded.get("http_headers").is_none());
}

#[test]
fn codex_entry_fields_and_document_validation_are_separate() {
    let server = json!({"type":"http", "url":"https://example.com/mcp",
        "headers":{"X-Test":"value"}, "extension":null});
    let native = McpConfigTarget::Codex
        .encode_server(&server)
        .expect("native field mapping");
    assert_eq!(native["http_headers"], server["headers"]);
    assert!(native.get("headers").is_none());
    assert_eq!(native.get("extension"), Some(&Value::Null));
    // TOML cannot represent null, so a validated document write still fails.
    assert!(project_mcp_server(
        &cc_switch_core::AppType::Codex,
        None,
        "test",
        McpServerProjection::Enable(&server)
    )
    .is_err());
}
