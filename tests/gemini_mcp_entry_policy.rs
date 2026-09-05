use cc_switch_core::{
    builtin_app_adapters, McpConfigError, McpConfigTarget, McpEntryDecodePolicy,
    McpEntryEncodePolicy,
};
use serde_json::{json, Value};

fn decode(value: &Value) -> Value {
    McpConfigTarget::Gemini
        .decode_server_with_policy(value, McpEntryDecodePolicy::InferFromStringFields)
        .expect("Gemini object")
}

fn encode(value: &Value) -> Value {
    McpConfigTarget::Gemini
        .encode_server_with_policy(value, McpEntryEncodePolicy::PreserveFields)
        .expect("Gemini object")
}

#[test]
fn registered_default_codecs_and_unsupported_policies_remain_explicit() {
    for adapter in builtin_app_adapters() {
        let Some(target) = adapter.mcp_config_target() else {
            continue;
        };
        for input in [
            json!({"command":"node", "future":{"keep":true}}),
            json!({"type":"http", "url":"https://example.com", "timeout":42}),
            json!({"type":false}),
            Value::Null,
        ] {
            assert_eq!(
                target.decode_server(&input),
                target.decode_server_with_policy(&input, McpEntryDecodePolicy::Preserve)
            );
            assert_eq!(
                target.encode_server(&input),
                target.encode_server_with_policy(&input, McpEntryEncodePolicy::Canonical)
            );
            if target != McpConfigTarget::Gemini {
                assert!(
                    matches!(target.decode_server_with_policy(&input, McpEntryDecodePolicy::InferFromStringFields),
                    Err(McpConfigError::UnsupportedEntryPolicy {target: actual}) if actual == target)
                );
                assert!(
                    matches!(target.encode_server_with_policy(&input, McpEntryEncodePolicy::PreserveFields),
                    Err(McpConfigError::UnsupportedEntryEncodingPolicy {target: actual}) if actual == target)
                );
            }
        }
    }
}

#[test]
fn string_inference_preserves_explicit_strings_and_native_alias_precedence() {
    for (input, expected) in [
        (
            json!({"command":"", "url":"remote", "type":false}),
            json!({"command":"", "url":"remote", "type":"stdio"}),
        ),
        (
            json!({"command":false, "url":"", "type":null}),
            json!({"command":false, "url":"", "type":"sse"}),
        ),
        (
            json!({"command":false, "url":42, "type":[]}),
            json!({"command":false, "url":42, "type":[]}),
        ),
        (
            json!({"command":"node", "type":"future"}),
            json!({"command":"node", "type":"future"}),
        ),
        (
            json!({"command":"node", "type":""}),
            json!({"command":"node", "type":""}),
        ),
        (
            json!({"httpUrl":null, "url":"old", "command":"node", "type":"future"}),
            json!({"url":null, "command":"node", "type":"http"}),
        ),
        (
            json!({"httpUrl":{}, "extension":{"nested":[null,42]}}),
            json!({"url":{}, "type":"http", "extension":{"nested":[null,42]}}),
        ),
    ] {
        let original = input.clone();
        assert_eq!(decode(&input), expected);
        assert_eq!(input, original);
    }
    // Existing callers still infer by field presence, and retain explicit types.
    assert_eq!(
        McpConfigTarget::Gemini
            .decode_server(&json!({"command":false, "url":"remote"}))
            .unwrap(),
        json!({"command":false, "url":"remote", "type":"stdio"})
    );
    assert_eq!(
        McpConfigTarget::Gemini
            .decode_server(&json!({"command":"node", "type":false}))
            .unwrap(),
        json!({"command":"node", "type":false})
    );
}

#[test]
fn preserving_encoder_keeps_rich_fields_without_catalog_selection() {
    // Synthetic third-consumer contract fixture, not full-product parity evidence.
    let input = json!({"type":"http", "url":"https://example.com/mcp", "httpUrl":"old",
        "command":"native-extension", "args":[null,"arg"], "env":{"OPAQUE":42},
        "headers":{"Authorization":"synthetic-token"}, "oauth":{"scopes":["read"]},
        "enabled":false, "name":"catalog-owned", "server":{"unconsumed":true},
        "startup_timeout_sec":1.5, "startup_timeout_ms":900_000, "tool_timeout_ms":2});
    let expected = json!({"httpUrl":"https://example.com/mcp",
        "command":"native-extension", "args":[null,"arg"], "env":{"OPAQUE":42},
        "headers":{"Authorization":"synthetic-token"}, "oauth":{"scopes":["read"]},
        "enabled":false, "name":"catalog-owned", "server":{"unconsumed":true},
        "startup_timeout_ms":900_000, "timeout":1500});
    let original = input.clone();
    assert_eq!(encode(&input), expected);
    assert_eq!(input, original);
    for typ in [json!(false), json!(42), json!("future"), Value::Null] {
        assert_eq!(
            encode(&json!({"type":typ, "url":false, "httpUrl":42})),
            json!({"url":false, "httpUrl":42, "timeout":60000})
        );
    }
}

#[test]
fn preserving_timeouts_use_seconds_precedence_and_saturating_milliseconds() {
    for (input, expected) in [
        (json!({}), json!({"timeout":60000})),
        (
            json!({"timeout":100000, "startup_timeout_sec":0, "tool_timeout_sec":0}),
            json!({"timeout":100000}),
        ),
        (
            json!({"startup_timeout_sec":false, "startup_timeout_ms":120000}),
            json!({"timeout":120000}),
        ),
        (
            json!({"startup_timeout_sec":-1, "startup_timeout_ms":120000, "tool_timeout_sec":-0.5, "tool_timeout_ms":90000}),
            json!({"startup_timeout_ms":120000, "tool_timeout_ms":90000, "timeout":0}),
        ),
        (
            json!({"startup_timeout_sec":0.0019, "tool_timeout_ms":0.9, "timeout":0.5}),
            json!({"timeout":1}),
        ),
        (
            json!({"startup_timeout_sec":u64::MAX}),
            json!({"timeout":u64::MAX}),
        ),
        (
            json!({"tool_timeout_sec":f64::MAX}),
            json!({"timeout":u64::MAX}),
        ),
        (
            json!({"timeout":-1, "startup_timeout_sec":"120", "startup_timeout_ms":null,
            "tool_timeout_sec":{}, "tool_timeout_ms":[]}),
            json!({"timeout":60000}),
        ),
    ] {
        assert_eq!(encode(&input), expected);
    }
    // Canonical projection keeps its max-of-both-unit rules.
    assert_eq!(
        McpConfigTarget::Gemini
            .encode_server(&json!({"command":"node",
        "startup_timeout_sec":1, "startup_timeout_ms":120000, "tool_timeout_ms":1}))
            .unwrap(),
        json!({"command":"node", "timeout":120000})
    );
}

#[test]
fn entry_policies_do_not_validate_connections_or_impose_document_bounds() {
    assert_eq!(decode(&json!({})), json!({}));
    assert_eq!(encode(&json!({})), json!({"timeout":60000}));
    let large = "x".repeat(cc_switch_core::MAX_OPERATION_CONTENT_BYTES + 1);
    let input = json!({"future":large});
    assert_eq!(decode(&input), input);
    assert_eq!(encode(&input)["future"], input["future"]);
    for input in [Value::Null, json!(false), json!(42), json!([])] {
        assert!(McpConfigTarget::Gemini
            .decode_server_with_policy(&input, McpEntryDecodePolicy::InferFromStringFields)
            .is_err());
        assert!(McpConfigTarget::Gemini
            .encode_server_with_policy(&input, McpEntryEncodePolicy::PreserveFields)
            .is_err());
    }
}
