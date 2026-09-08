use super::*;
use serde_json::json;

#[test]
fn entry_restoration_matches_document_restoration_for_removable_targets() {
    for (app, target) in [
        (AppType::Claude, McpConfigTarget::Claude),
        (AppType::Gemini, McpConfigTarget::Gemini),
    ] {
        let entry = r#"{"command":42,"trust":"keep","exact":9007199254740993.0,"timeout":123456}"#;
        let snapshot = target.capture_native_entry(entry).unwrap();
        let document = format!(r#"{{"mcpServers":{{"same":{entry}}}}}"#);
        let existing = capture_mcp_native_snapshot(&app, Some(document.as_bytes()), "same")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot, existing);
        let server = json!({"command":"not-executed","args":["fixture"]});
        let restored = target
            .restore_native_entry_with_policy(&snapshot, &server, McpEntryEncodePolicy::Canonical)
            .unwrap();
        let projected = project_mcp_server(
            &app,
            None,
            "same",
            McpServerProjection::Restore {
                server: &server,
                snapshot: &snapshot,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            json_patch::object_entry(
                &json_patch::object_entry(&projected, "mcpServers")
                    .unwrap()
                    .unwrap(),
                "same"
            )
            .unwrap()
            .unwrap(),
            restored,
        );
        assert!(restored.contains("9007199254740993.0"));
        assert!(!format!("{snapshot:?}").contains("trust"));
    }
}

#[test]
fn entry_snapshots_reject_invalid_shapes_cross_target_and_unsupported_policies() {
    for entry in ["[]", "null", "42", "{broken"] {
        assert!(McpConfigTarget::Gemini.capture_native_entry(entry).is_err());
    }
    for descriptor in crate::builtin_app_registry().descriptors() {
        if let Some(contract) = descriptor.mcp_contract() {
            let target = contract.target();
            assert_eq!(
                target.capture_native_entry("{}").is_ok(),
                matches!(target, McpConfigTarget::Claude | McpConfigTarget::Gemini)
            );
        }
    }
    let snapshot = McpConfigTarget::Claude.capture_native_entry("{}").unwrap();
    let server = json!({"command":"not-executed"});
    assert!(McpConfigTarget::Gemini
        .restore_native_entry_with_policy(&snapshot, &server, McpEntryEncodePolicy::Canonical)
        .is_err());
    assert!(McpConfigTarget::Claude
        .restore_native_entry_with_policy(&snapshot, &Value::Null, McpEntryEncodePolicy::Canonical)
        .is_err());
}

#[test]
fn preserving_entry_policy_keeps_host_tolerance_and_host_content_limits() {
    let target = McpConfigTarget::Gemini;
    let entry = json!({"private": "x".repeat(MAX_OPERATION_CONTENT_BYTES + 1), "timeout": 123456});
    let snapshot = target.capture_native_entry(&entry.to_string()).unwrap();
    let server = json!({"type":"unknown", "command":42, "custom":"keep"});
    let encoded = target
        .encode_server_with_policy(&server, McpEntryEncodePolicy::PreserveFields)
        .unwrap();
    let restored: Value = serde_json::from_str(
        &target
            .restore_native_entry_with_policy(
                &snapshot,
                &server,
                McpEntryEncodePolicy::PreserveFields,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(restored["private"], entry["private"]);
    assert_eq!(restored["timeout"], entry["timeout"]);
    for key in ["command", "type", "custom"] {
        assert_eq!(restored[key], encoded[key]);
    }
    // Document APIs retain their original default limit.
    let document = json!({"mcpServers":{"same":entry}}).to_string();
    assert!(
        capture_mcp_native_snapshot(&AppType::Gemini, Some(document.as_bytes()), "same").is_err()
    );
}
