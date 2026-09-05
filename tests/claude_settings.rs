use cc_switch_core::claude::{
    normalize_model_keys, prepare_live_settings, prepare_live_snapshot, strip_internal_metadata,
};
use serde_json::{json, Value};

#[test]
fn model_migration_preserves_native_extensions_and_existing_role_values() {
    let mut settings = json!({
        "api_format": "host-metadata",
        "env": {
            "ANTHROPIC_MODEL": " main ",
            "ANTHROPIC_SMALL_FAST_MODEL": "fast",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": null,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": {"future": true},
            "ANTHROPIC_AUTH_TOKEN": "opaque-token"
        },
        "permissions": {"allow": ["Bash(git*)"]},
        "hooks": {"future": [{"apiFormat": "keep"}]}
    });
    let mut expected = settings.clone();
    expected["env"]
        .as_object_mut()
        .unwrap()
        .remove("ANTHROPIC_SMALL_FAST_MODEL");
    expected["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"] = json!("fast");
    assert!(normalize_model_keys(&mut settings));
    assert_eq!(settings, expected);
    assert!(!normalize_model_keys(&mut settings));

    let mut invalid_legacy = json!({"env": {"ANTHROPIC_SMALL_FAST_MODEL": false}});
    assert!(normalize_model_keys(&mut invalid_legacy));
    assert_eq!(invalid_legacy, json!({"env": {}}));
    assert!(!normalize_model_keys(&mut invalid_legacy));
}

#[test]
fn model_fallbacks_are_explicit_and_do_not_trim_strings() {
    for (source, value) in [
        ("ANTHROPIC_MODEL", " main "),
        ("ANTHROPIC_SMALL_FAST_MODEL", ""),
    ] {
        let mut env = serde_json::Map::new();
        env.insert(source.into(), json!(value));
        let mut settings = json!({"env": env});
        assert_eq!(prepare_live_settings(&settings), settings);
        assert_eq!(prepare_live_snapshot(&settings).unwrap().settings, settings);
        assert!(normalize_model_keys(&mut settings));
        for role in ["HAIKU", "SONNET", "OPUS"] {
            assert_eq!(
                settings["env"][format!("ANTHROPIC_DEFAULT_{role}_MODEL")],
                value
            );
        }
    }
}

#[test]
fn transforms_tolerate_non_objects_and_metadata_cleanup_is_shallow() {
    for settings in [
        Value::Null,
        json!(false),
        json!(12),
        json!("value"),
        json!([]),
        json!({}),
        json!({"env": null}),
        json!({"env": []}),
        json!({"env": "invalid"}),
        json!({"env": false}),
    ] {
        let mut live = settings.clone();
        assert!(!normalize_model_keys(&mut live));
        strip_internal_metadata(&mut live);
        assert_eq!(live, settings);
    }
    let mut settings = json!({
        "api_format": null, "apiFormat": [],
        "openrouter_compat_mode": {}, "openrouterCompatMode": false,
        "env": {"api_format": "nested", "ANTHROPIC_SMALL_FAST_MODEL": "keep"},
        "future": {"oauth": ["opaque"], "openrouterCompatMode": true}
    });
    let expected = json!({
        "env": {"api_format": "nested", "ANTHROPIC_SMALL_FAST_MODEL": "keep"},
        "future": {"oauth": ["opaque"], "openrouterCompatMode": true}
    });
    strip_internal_metadata(&mut settings);
    assert_eq!(settings, expected);
    strip_internal_metadata(&mut settings);
    assert_eq!(settings, expected);
}
