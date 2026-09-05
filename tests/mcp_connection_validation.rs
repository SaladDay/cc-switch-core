use cc_switch_core::{
    builtin_app_adapters, validate_mcp_connection, validate_mcp_server,
    validate_mcp_server_for_app, McpConfigError, McpConnectionError, MAX_OPERATION_CONTENT_BYTES,
};
use serde_json::{json, Value};

#[test]
fn connection_errors_distinguish_transport_and_required_field_failures() {
    for (input, expected) in [
        (Value::Null, McpConnectionError::NotObject),
        (json!([]), McpConnectionError::NotObject),
        (json!({"type":false}), McpConnectionError::NonStringType),
        (json!({"type":null}), McpConnectionError::NonStringType),
        (
            json!({"type":""}),
            McpConnectionError::UnsupportedTransport("".into()),
        ),
        (
            json!({"type":"HTTP"}),
            McpConnectionError::UnsupportedTransport("HTTP".into()),
        ),
        (json!({}), McpConnectionError::MissingCommand),
        (json!({"url":"remote"}), McpConnectionError::MissingCommand),
        (json!({"command":42}), McpConnectionError::MissingCommand),
        (
            json!({"command":" \t\n\u{2003}"}),
            McpConnectionError::MissingCommand,
        ),
        (
            json!({"type":"http", "command":"node"}),
            McpConnectionError::MissingHttpUrl,
        ),
        (
            json!({"type":"http", "url":[]}),
            McpConnectionError::MissingHttpUrl,
        ),
        (
            json!({"type":"sse", "url":false}),
            McpConnectionError::MissingSseUrl,
        ),
        (
            json!({"type":"sse", "url":"\u{a0}"}),
            McpConnectionError::MissingSseUrl,
        ),
    ] {
        assert_eq!(validate_mcp_connection(&input), Err(expected), "{input}");
    }
    for input in [
        json!({"command":" node "}),
        json!({"type":"stdio", "command":"\u{200b}"}),
        json!({"type":"http", "url":"not a URL"}),
        json!({"type":"sse", "url":" /local "}),
    ] {
        validate_mcp_connection(&input).unwrap();
    }
}

#[test]
fn connection_check_leaves_rich_native_fields_to_the_host() {
    // Synthetic third-consumer contract, not a full-product compatibility fixture.
    let input = json!({"type":"http", "url":"https://example.com/mcp",
        "command":"opaque", "args":[42], "env":{"TOKEN":false}, "cwd":null,
        "http_headers":{"native":true}, "headers":["opaque"], "timeout":-1,
        "oauth":{"scopes":["read"]}, "enabled":false, "server":{"keep":true},
        "extension":{"nested":[null,42]}});
    let original = serde_json::to_string(&input).unwrap();
    assert_eq!(validate_mcp_connection(&input), Ok(()));
    assert!(validate_mcp_server("rich", &input).is_err());
    assert_eq!(serde_json::to_string(&input).unwrap(), original);

    let large = json!({"command":"node", "opaque":"x".repeat(MAX_OPERATION_CONTENT_BYTES)});
    assert_eq!(validate_mcp_connection(&large), Ok(()));
    assert_eq!(
        validate_mcp_server("large", &large),
        Err(McpConfigError::InvalidServer(format!(
            "definition exceeds {MAX_OPERATION_CONTENT_BYTES} bytes"
        )))
    );
}

#[test]
fn strict_validation_retains_error_order_and_optional_field_rules() {
    for (input, message) in [
        (Value::Null, "the definition must be an object"),
        (
            json!({"environment":{}, "http_headers":{}, "type":false}),
            "'environment' is a native field; use canonical 'env'",
        ),
        (
            json!({"http_headers":{}, "httpUrl":"remote", "type":false}),
            "'http_headers' is a native field; use canonical 'headers'",
        ),
        (
            json!({"httpUrl":"remote", "type":false}),
            "'httpUrl' is a native field; use canonical 'url'",
        ),
        (
            json!({"type":false, "args":false}),
            "'type' must be a string",
        ),
        (
            json!({"type":"future", "args":false}),
            "unsupported transport 'future'",
        ),
        (
            json!({"args":false, "env":false}),
            "stdio definitions require command",
        ),
        (
            json!({"command":"node", "args":false, "env":false}),
            "'args' must contain only strings",
        ),
        (
            json!({"command":"node", "env":false, "cwd":false}),
            "'env' must map strings to strings",
        ),
        (
            json!({"command":"node", "cwd":false, "url":"remote"}),
            "'cwd' must be a string",
        ),
        (
            json!({"command":"node", "url":"remote", "headers":false}),
            "'url' is not valid for 'stdio' definitions",
        ),
        (
            json!({"type":"http", "headers":false}),
            "remote definitions require url",
        ),
        (
            json!({"type":"sse", "url":"\t", "headers":false}),
            "remote definitions require url",
        ),
        (
            json!({"type":"http", "url":"remote", "headers":false, "command":"node"}),
            "'headers' must map strings to strings",
        ),
        (
            json!({"type":"sse", "url":"remote", "command":"node", "args":[]}),
            "'command' is not valid for 'sse' definitions",
        ),
    ] {
        let expected = Err(McpConfigError::InvalidServer(message.to_owned()));
        assert_eq!(validate_mcp_server("server", &input), expected, "{input}");
        for adapter in builtin_app_adapters().filter(|adapter| adapter.mcp_contract().is_some()) {
            assert_eq!(
                validate_mcp_server_for_app(adapter.descriptor().app(), "server", &input),
                expected,
                "{}: {input}",
                adapter.descriptor().app().as_str()
            );
        }
        assert!(matches!(
            validate_mcp_server(" ", &input),
            Err(McpConfigError::InvalidId(_))
        ));
    }
    for adapter in builtin_app_adapters().filter(|adapter| adapter.mcp_contract().is_some()) {
        validate_mcp_server_for_app(
            adapter.descriptor().app(),
            "server",
            &json!({"command":"node"}),
        )
        .unwrap();
    }
}
