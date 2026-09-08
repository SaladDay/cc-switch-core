use cc_switch_core::{builtin_app_adapters, import_mcp_servers, McpConfigError, McpConfigTarget};
use serde_json::{json, Value};
use std::fmt::Write;

#[test]
fn every_registered_entry_flag_agrees_with_import_without_document_overrides() {
    for adapter in builtin_app_adapters() {
        let Some(target) = adapter.mcp_config_target() else {
            continue;
        };
        let app = adapter.descriptor().app();
        let preserves_disabled = adapter
            .descriptor()
            .mcp_contract()
            .unwrap()
            .preserves_disabled_entry();
        for server in [
            json!({"command":"not-executed"}),
            json!({"type":"http","url":"https://example.invalid/mcp"}),
        ] {
            for flag in [
                None,
                Some(json!(true)),
                Some(json!(false)),
                Some(json!("false")),
                Some(json!(0)),
                Some(json!([])),
                Some(json!({})),
            ] {
                let mut entry = target.encode_server(&server).unwrap();
                entry.as_object_mut().unwrap().remove("enabled");
                if let Some(flag) = &flag {
                    entry["enabled"] = flag.clone();
                }
                let before = entry.clone();
                let state = target.entry_enabled_flag(&entry);
                assert_eq!(entry, before);
                let strict = matches!(target, McpConfigTarget::Codex | McpConfigTarget::GrokBuild);
                let invalid = flag.as_ref().is_some_and(|flag| !flag.is_boolean());
                let section = match target {
                    McpConfigTarget::Claude | McpConfigTarget::Gemini => "mcpServers",
                    McpConfigTarget::OpenCode => "mcp",
                    _ => "mcp_servers",
                };
                let root = json!({section:{"test":entry}});
                let document = match target {
                    McpConfigTarget::Codex | McpConfigTarget::GrokBuild => {
                        // These fixture values are strings, booleans, integers
                        // and empty containers, whose literals also fit TOML.
                        let mut document = "[mcp_servers.test]\n".to_owned();
                        for (key, value) in root[section]["test"].as_object().unwrap() {
                            writeln!(document, "{key} = {value}").unwrap();
                        }
                        document
                    }
                    McpConfigTarget::Hermes => serde_yaml::to_string(&root).unwrap(),
                    _ => serde_json::to_string(&root).unwrap(),
                };
                let imports = import_mcp_servers(app, Some(document.as_bytes()));
                if strict && invalid {
                    assert!(matches!(state, Err(McpConfigError::InvalidServer(_))));
                    assert!(matches!(
                        imports,
                        Err(McpConfigError::InvalidDocument { .. })
                    ));
                } else {
                    let expected = !preserves_disabled || flag != Some(json!(false));
                    assert_eq!(state.unwrap(), expected, "{target:?}: {flag:?}");
                    let imports = imports.unwrap();
                    assert_eq!(imports.len(), 1, "{target:?}: {flag:?}");
                    assert_eq!(imports[0].enabled, expected);
                }
            }
        }
    }
}

#[test]
fn state_reading_is_not_connection_validation_or_absence_detection() {
    for adapter in builtin_app_adapters() {
        let Some(target) = adapter.mcp_config_target() else {
            continue;
        };
        assert!(target.entry_enabled_flag(&json!({})).unwrap());
        assert!(target.entry_enabled_flag(&json!({"command":42})).unwrap());
        for non_object in [
            Value::Null,
            json!([]),
            json!(true),
            json!(42),
            json!("entry"),
        ] {
            assert!(matches!(
                target.entry_enabled_flag(&non_object),
                Err(McpConfigError::InvalidServer(_))
            ));
        }
        let null_flag = target.entry_enabled_flag(&json!({"enabled":null}));
        if matches!(target, McpConfigTarget::Codex | McpConfigTarget::GrokBuild) {
            assert!(null_flag.is_err());
        } else {
            assert!(null_flag.unwrap());
        }
    }
}

#[test]
fn grok_effective_state_applies_document_overrides_after_entry_flags() {
    for flag in [None, Some(false), Some(true)] {
        for disabled_id in ["test", "other"] {
            let mut entry = json!({"command":"not-executed"});
            let fields = if let Some(flag) = flag {
                entry["enabled"] = json!(flag);
                format!("enabled = {flag}\n")
            } else {
                String::new()
            };
            let local = McpConfigTarget::GrokBuild
                .entry_enabled_flag(&entry)
                .unwrap();
            assert_eq!(local, flag.unwrap_or(true));
            let document = format!("disabled_mcp_servers = [\"{disabled_id}\"]\n[mcp_servers.test]\ncommand = \"not-executed\"\n{fields}");
            let imports = import_mcp_servers(
                &cc_switch_core::AppType::GrokBuild,
                Some(document.as_bytes()),
            )
            .unwrap();
            assert_eq!(imports.len(), 1);
            assert_eq!(imports[0].enabled, local && disabled_id != "test");
        }
    }
}
