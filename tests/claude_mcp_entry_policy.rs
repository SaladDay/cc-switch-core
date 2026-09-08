use cc_switch_core::{McpConfigTarget, McpEntryEncodePolicy, MAX_OPERATION_CONTENT_BYTES};
use serde_json::{json, Value};

const CLAUDE: McpConfigTarget = McpConfigTarget::Claude;

#[test]
fn preserving_encoding_keeps_native_fields_without_catalog_interpretation() {
    // Synthetic rich-native contract, not a full-product compatibility fixture.
    let input = json!({"type":"future", "command":42, "args":[null,"arg"],
        "env":{"OPAQUE":false}, "url":null, "headers":{"Authorization":"fixture"},
        "server":{"native":{"keep":true}}, "enabled":false, "source":"native",
        "id":"native-id", "name":"native-name", "description":"native-description",
        "tags":["native-tag"], "homepage":"native-homepage", "docs":"native-docs",
        "oauth":{"scopes":["read"]}});
    let original = input.clone();
    let encoded = CLAUDE
        .encode_server_with_policy(&input, McpEntryEncodePolicy::PreserveFields)
        .unwrap();
    assert_eq!(encoded.to_string(), input.to_string());
    assert_eq!(input, original);
    let mut canonical = input.clone();
    for key in [
        "server",
        "enabled",
        "source",
        "id",
        "name",
        "description",
        "tags",
        "homepage",
        "docs",
    ] {
        canonical.as_object_mut().unwrap().shift_remove(key);
    }
    assert_eq!(CLAUDE.encode_server(&input).unwrap(), canonical);
    assert_eq!(
        CLAUDE
            .encode_server_with_policy(&input, McpEntryEncodePolicy::Canonical)
            .unwrap(),
        canonical
    );
}

#[test]
fn host_selected_nested_server_extension_survives_encoding_and_restoration() {
    // The CLI baseline unwraps once, then removes these UI fields. The inner
    // `server` belongs to native data, not another catalog-wrapper layer.
    let catalog = json!({"name":"outer", "server":{
        "command":"not-executed", "name":"inner-ui", "enabled":false,
        "server":{"native-only":true}, "future":[null,42]}});
    let mut selected = catalog["server"].clone();
    for key in [
        "enabled",
        "source",
        "id",
        "name",
        "description",
        "tags",
        "homepage",
        "docs",
    ] {
        selected.as_object_mut().unwrap().shift_remove(key);
    }
    let expected =
        json!({"command":"not-executed", "server":{"native-only":true}, "future":[null,42]});
    assert_eq!(
        CLAUDE
            .encode_server_with_policy(&selected, McpEntryEncodePolicy::PreserveFields)
            .unwrap(),
        expected
    );
    let snapshot = CLAUDE.capture_native_entry(r#"{"trust":true}"#).unwrap();
    let restored: Value = serde_json::from_str(
        &CLAUDE
            .restore_native_entry_with_policy(
                &snapshot,
                &selected,
                McpEntryEncodePolicy::PreserveFields,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        restored,
        json!({"command":"not-executed", "server":{"native-only":true}, "future":[null,42], "trust":true})
    );
}

#[test]
fn restoration_retains_native_extensions_and_replaces_current_connection() {
    let snapshot = CLAUDE
        .capture_native_entry(
            r#"{
        "type":"stdio", "command":"old", "args":["old"], "env":{"OLD":"old"},
        "cwd":"old", "url":"old", "headers":{"OLD":"old"},
        "server":{"native":true}, "name":"native", "exact":9007199254740993.0
    }"#,
        )
        .unwrap();
    let incoming = json!({"type":"http", "url":"https://example.invalid/mcp", "headers":{"NEW":"fixture"},
        "server":{"incoming":true}, "name":"incoming", "new-extension":[null,42]});
    let restored = CLAUDE
        .restore_native_entry_with_policy(
            &snapshot,
            &incoming,
            McpEntryEncodePolicy::PreserveFields,
        )
        .unwrap();
    let value: Value = serde_json::from_str(&restored).unwrap();
    for key in ["command", "args", "env", "cwd"] {
        assert!(value.get(key).is_none());
    }
    for key in ["type", "url", "headers", "new-extension"] {
        assert_eq!(value[key], incoming[key]);
    }
    assert_eq!(value["server"], json!({"native":true}));
    assert_eq!(value["name"], "native");
    assert!(restored.contains("9007199254740993.0"));
}

#[test]
fn preserving_policy_keeps_shape_errors_and_leaves_size_policy_to_the_host() {
    let snapshot = CLAUDE.capture_native_entry("{}").unwrap();
    for value in [
        Value::Null,
        json!([]),
        json!(42),
        json!("text"),
        json!(false),
    ] {
        assert!(CLAUDE
            .encode_server_with_policy(&value, McpEntryEncodePolicy::PreserveFields)
            .is_err());
        assert!(CLAUDE
            .restore_native_entry_with_policy(
                &snapshot,
                &value,
                McpEntryEncodePolicy::PreserveFields
            )
            .is_err());
    }
    let other = McpConfigTarget::Gemini.capture_native_entry("{}").unwrap();
    assert!(CLAUDE
        .restore_native_entry_with_policy(&other, &json!({}), McpEntryEncodePolicy::PreserveFields)
        .is_err());
    let large =
        json!({"type":false, "command":null, "native":"x".repeat(MAX_OPERATION_CONTENT_BYTES + 1)});
    assert_eq!(
        CLAUDE
            .encode_server_with_policy(&large, McpEntryEncodePolicy::PreserveFields)
            .unwrap(),
        large
    );
    let restored: Value = serde_json::from_str(
        &CLAUDE
            .restore_native_entry_with_policy(
                &snapshot,
                &large,
                McpEntryEncodePolicy::PreserveFields,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(restored, large);
}
