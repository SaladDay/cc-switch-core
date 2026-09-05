use cc_switch_core::{
    builtin_app_adapters, validate_mcp_server, McpConfigError, McpConfigTarget,
    McpEntryDecodePolicy,
};
use serde_json::{json, Value};

#[test]
fn explicit_preserve_policy_matches_every_registered_codec() {
    for adapter in builtin_app_adapters() {
        let Some(target) = adapter.mcp_config_target() else {
            continue;
        };
        for entry in [
            json!({"command":"node", "enabled":false, "future":{"keep":true}}),
            json!({"url":"https://example.com/mcp", "http_headers":{"X-Test":"value"}}),
            json!({"type":false}),
            Value::Null,
        ] {
            assert_eq!(
                target.decode_server(&entry),
                target.decode_server_with_policy(&entry, McpEntryDecodePolicy::Preserve),
                "{target:?}"
            );
            if target != McpConfigTarget::Codex {
                assert!(matches!(
                    target.decode_server_with_policy(&entry, McpEntryDecodePolicy::TransportFields),
                    Err(McpConfigError::UnsupportedEntryPolicy { target: actual }) if actual == target
                ));
            }
        }
    }
}

fn decode(entry: &Value) -> Value {
    McpConfigTarget::Codex
        .decode_server_with_policy(entry, McpEntryDecodePolicy::TransportFields)
        .expect("Codex entry object")
}

#[test]
fn transport_fields_keep_tolerant_types_and_native_alias_precedence() {
    for (entry, expected) in [
        (
            json!({"command":"node", "args":["server",42,false,{},[]],
                "env":{"KEEP":"value", "DROP":42}, "cwd":" /work ",
                "headers":{"EXTENSION":"not a stdio field"}, "enabled":false}),
            json!({"type":"stdio", "command":"node", "args":["server"],
                "cwd":" /work ", "env":{"KEEP":"value"}}),
        ),
        (
            json!({"url":"https://example.com/mcp", "command":"not selected",
                "http_headers":{"KEEP":"native", "DROP":false}, "headers":{"LEGACY":"ignored"}}),
            json!({"type":"http", "url":"https://example.com/mcp", "headers":{"KEEP":"native"}}),
        ),
        (
            json!({"type":"sse", "url":"https://example.com/sse", "http_headers":{},
                "headers":{"LEGACY":"ignored"}, "env":{"EXTENSION":"ignored"}}),
            json!({"type":"sse", "url":"https://example.com/sse"}),
        ),
        (
            json!({"type":"http", "url":"", "http_headers":false,
                "headers":{"KEEP":"legacy", "DROP":42}}),
            json!({"type":"http", "url":"", "headers":{"KEEP":"legacy"}}),
        ),
        (
            json!({"command":false, "args":[42], "env":{}, "cwd":"\u{2003}"}),
            json!({"type":"stdio"}),
        ),
    ] {
        let original = entry.clone();
        assert_eq!(decode(&entry), expected);
        assert_eq!(entry, original, "decoding must not mutate its input");
    }
}

#[test]
fn absent_types_are_inferred_but_invalid_types_still_need_validation() {
    for url in [
        Value::Null,
        json!(42),
        json!(false),
        json!([]),
        json!({}),
        json!(""),
        json!("\u{2003}"),
    ] {
        assert_eq!(
            decode(&json!({"url":url, "command":"node"})),
            json!({"type":"stdio", "command":"node"})
        );
    }
    for typ in [
        Value::Null,
        json!(42),
        json!(false),
        json!([]),
        json!({}),
        json!("future"),
    ] {
        let decoded =
            decode(&json!({"type":typ, "url":"https://example.com/mcp", "command":"node"}));
        assert_eq!(decoded, json!({"type":typ}));
        assert!(validate_mcp_server("test", &decoded).is_err());
    }
    // The codec may represent an incomplete transport; validation stays separate.
    assert_eq!(decode(&json!({})), json!({"type":"stdio"}));
    assert!(validate_mcp_server("test", &decode(&json!({}))).is_err());
    for malformed in [Value::Null, json!([]), json!(false), json!(42)] {
        assert!(McpConfigTarget::Codex
            .decode_server_with_policy(&malformed, McpEntryDecodePolicy::TransportFields)
            .is_err());
    }
}
