use cc_switch_core::gemini::{
    prepare_live_snapshot, select_string_env_values, AuthMode, PrepareLiveSnapshotError,
    PreparedLiveSnapshot, SettingsOverlay,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[test]
fn default_gemini_preparation_matches_previous_values_errors_and_order() {
    let envs = [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!({})),
        Some(json!({"GEMINI_API_KEY":""})),
        Some(json!({"GEMINI_API_KEY":7})),
        Some(json!({"GEMINI_API_KEY":"fake","变量":"literal\0value","non_string":false})),
        Some(json!({"Z":"last","GEMINI_API_KEY":"fake","A":"first"})),
    ];
    let configs = [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!(false)),
        Some(json!({})),
        Some(json!({"security":null})),
        Some(json!({"security":{"auth":17}})),
        Some(
            json!({"theme":"dark","security":{"auth":{"future":"keep"}},"advanced":{"opaque":[1,2]}}),
        ),
    ];
    let existing = [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!({})),
        Some(json!({"security":false})),
        Some(json!({"security":{"auth":false}})),
        Some(
            json!({"first":1,"theme":"light","security":{"old":"keep","auth":{"token":"fake","selectedType":"old"}},"mcpServers":{"tool":{"command":"fake"}},"last":2}),
        ),
    ];
    for env in &envs {
        for config in &configs {
            for existing in &existing {
                for mode in [AuthMode::ApiKey, AuthMode::OAuthPersonal] {
                    let mut input = json!({"future":{"untouched":true}});
                    if let Some(env) = env {
                        input["env"] = env.clone();
                    }
                    if let Some(config) = config {
                        input["config"] = config.clone();
                    }
                    let before = input.clone();
                    let expected = baseline_prepare(&input, existing.as_ref(), mode);
                    let actual = prepare_live_snapshot(&input, existing.as_ref(), mode);
                    assert_eq!(actual, expected);
                    if let (Ok(actual), Ok(expected)) = (actual, expected) {
                        assert_eq!(
                            serde_json::to_vec(&actual.settings).unwrap(),
                            serde_json::to_vec(&expected.settings).unwrap()
                        );
                    }
                    assert_eq!(input, before);
                }
            }
        }
    }
    for root in [json!(null), json!(false), json!([]), json!("opaque")] {
        assert_eq!(
            prepare_live_snapshot(&root, None, AuthMode::ApiKey),
            baseline_prepare(&root, None, AuthMode::ApiKey)
        );
    }
}

#[test]
fn settings_overlay_declares_top_level_ownership_and_auth_selection_stage() {
    let existing = json!({"before":1,"security":{"host":"old","auth":{"token":"fake"}},"theme":"light","mcpServers":{"keep":true},"after":2});
    for absent in [None, Some(&Value::Null)] {
        let overlay = SettingsOverlay::from_config(absent).unwrap();
        assert_eq!(
            Value::Object(overlay.apply_to(existing.as_object().unwrap().clone())),
            existing
        );
        let overlay = SettingsOverlay::from_config(absent)
            .unwrap()
            .with_auth_mode(AuthMode::OAuthPersonal)
            .unwrap();
        let output = Value::Object(overlay.apply_to(existing.as_object().unwrap().clone()));
        assert_eq!(
            output["security"],
            json!({"auth":{"selectedType":"oauth-personal"}})
        );
        assert_eq!(output["mcpServers"], existing["mcpServers"]);
    }
    let incoming = json!({"theme":"dark","advanced":{"opaque":"fake-secret"},"security":{"future":"keep","auth":{"extra":[1,2]}}});
    let overlay = SettingsOverlay::from_config(Some(&incoming))
        .unwrap()
        .with_auth_mode(AuthMode::ApiKey)
        .unwrap();
    assert!(!format!("{overlay:?}").contains("fake-secret"));
    let output = Value::Object(overlay.apply_to(existing.as_object().unwrap().clone()));
    assert_eq!(
        output["security"],
        json!({"future":"keep","auth":{"extra":[1,2],"selectedType":"gemini-api-key"}})
    );
    assert_eq!(output["advanced"], incoming["advanced"]);
    assert_eq!(output["mcpServers"], existing["mcpServers"]);
    assert_eq!(
        output
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "before",
            "security",
            "theme",
            "mcpServers",
            "after",
            "advanced"
        ]
    );
    // The registered default preparation instead selects auth after merging,
    // retaining existing security when incoming config owns no such field.
    let strict = prepare_live_snapshot(
        &json!({"env":{"GEMINI_API_KEY":"fake"}}),
        Some(&existing),
        AuthMode::ApiKey,
    )
    .unwrap();
    assert_eq!(strict.settings["security"]["host"], "old");
    assert_eq!(strict.settings["security"]["auth"]["token"], "fake");
}

#[test]
fn settings_overlay_errors_are_structural_and_do_not_expose_values() {
    for config in [json!(false), json!([]), json!("fake-secret")] {
        assert_eq!(
            SettingsOverlay::from_config(Some(&config)),
            Err(PrepareLiveSnapshotError::ConfigNotObject)
        );
    }
    for (config, field) in [
        (json!({"security":"fake-secret"}), "security"),
        (json!({"security":{"auth":"fake-secret"}}), "auth"),
    ] {
        let original = config.clone();
        let error = SettingsOverlay::from_config(Some(&config))
            .unwrap()
            .with_auth_mode(AuthMode::ApiKey)
            .unwrap_err();
        assert_eq!(
            error,
            PrepareLiveSnapshotError::SettingsFieldNotObject(field)
        );
        assert!(!format!("{error:?} {error}").contains("fake-secret"));
        assert_eq!(config, original);
    }
}

#[test]
fn env_field_selection_is_tolerant_but_does_not_relax_strict_preparation() {
    for value in [
        None,
        Some(json!(null)),
        Some(json!(false)),
        Some(json!("opaque")),
        Some(json!([])),
    ] {
        assert!(select_string_env_values(value.as_ref()).is_empty());
    }
    let env = json!({"GEMINI_API_KEY":"", "变量":"raw\0value","BAD-KEY":"x\ry","skip":9,"also_skip":null});
    assert_eq!(
        select_string_env_values(Some(&env)),
        BTreeMap::from([
            ("GEMINI_API_KEY".to_owned(), "".to_owned()),
            ("变量".to_owned(), "raw\0value".to_owned()),
            ("BAD-KEY".to_owned(), "x\ry".to_owned())
        ])
    );
    assert_eq!(
        prepare_live_snapshot(&json!({"env":env}), None, AuthMode::ApiKey),
        Err(PrepareLiveSnapshotError::EnvValueNotString)
    );
}

// Test-only oracle from Core 1153d83, independent of the new shared helpers.
fn baseline_prepare(
    provider_settings: &Value,
    existing_settings: Option<&Value>,
    auth_mode: AuthMode,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    let provider = provider_settings
        .as_object()
        .ok_or(PrepareLiveSnapshotError::SettingsNotObject)?;
    let env = project_env(provider.get("env"))?;
    if auth_mode == AuthMode::ApiKey
        && env
            .get("GEMINI_API_KEY")
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(PrepareLiveSnapshotError::MissingApiKey);
    }

    let mut settings = match existing_settings {
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err(PrepareLiveSnapshotError::ExistingSettingsNotObject),
        None => Value::Object(Map::new()),
    };
    match provider.get("config") {
        Some(Value::Object(config)) => {
            let target = settings
                .as_object_mut()
                .ok_or(PrepareLiveSnapshotError::ExistingSettingsNotObject)?;
            for (key, value) in config {
                target.insert(key.clone(), value.clone());
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(PrepareLiveSnapshotError::ConfigNotObject),
    }
    set_selected_auth_type(
        &mut settings,
        match auth_mode {
            AuthMode::ApiKey => "gemini-api-key",
            AuthMode::OAuthPersonal => "oauth-personal",
        },
    )?;

    Ok(PreparedLiveSnapshot { env, settings })
}

fn project_env(env: Option<&Value>) -> Result<BTreeMap<String, String>, PrepareLiveSnapshotError> {
    let Some(env) = env else {
        return Ok(BTreeMap::new());
    };
    let env = env
        .as_object()
        .ok_or(PrepareLiveSnapshotError::EnvNotObject)?;
    env.iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or(PrepareLiveSnapshotError::EnvValueNotString)
        })
        .collect()
}

fn set_selected_auth_type(
    settings: &mut Value,
    selected_type: &str,
) -> Result<(), PrepareLiveSnapshotError> {
    let settings = settings
        .as_object_mut()
        .ok_or(PrepareLiveSnapshotError::ExistingSettingsNotObject)?;
    let security = object_field(settings, "security")?;
    let auth = object_field(security, "auth")?;
    auth.insert(
        "selectedType".to_owned(),
        Value::String(selected_type.to_owned()),
    );
    Ok(())
}

fn object_field<'a>(
    parent: &'a mut Map<String, Value>,
    key: &'static str,
) -> Result<&'a mut Map<String, Value>, PrepareLiveSnapshotError> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or(PrepareLiveSnapshotError::SettingsFieldNotObject(key))
}
