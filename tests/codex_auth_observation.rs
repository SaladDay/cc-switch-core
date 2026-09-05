use cc_switch_core::codex::{
    auth_has_credential_login_material, auth_has_login_material, observe_auth,
};
use serde_json::{json, Value};

#[test]
fn auth_observations_separate_opaque_payload_from_credentials() {
    for (auth, key, credentials, other_payload) in [
        (Value::Null, false, false, false),
        (json!([]), false, false, false),
        (json!(false), false, false, false),
        (json!({"auth_mode":"chatgpt"}), false, false, false),
        (json!({"OPENAI_API_KEY":"  "}), false, false, false),
        (json!({"OPENAI_API_KEY":42}), false, false, false),
        (json!({"OPENAI_API_KEY":" test-key "}), true, false, false),
        (json!({"last_refresh":"2026-09-05"}), false, false, true),
        (json!({"tokens":{}}), false, false, false),
        (json!({"tokens":{"access_token":42}}), false, false, true),
        (
            json!({"tokens":{"access_token":"test-token"}}),
            false,
            true,
            true,
        ),
        (json!({"personal_access_token":false}), false, false, true),
        (
            json!({"personal_access_token":"test-token"}),
            false,
            true,
            true,
        ),
        (
            json!({"agent_identity":{"agent_runtime_id":"id"}}),
            false,
            false,
            true,
        ),
        (
            json!({"agent_identity":{"agent_runtime_id":"id","agent_private_key":"key"}}),
            false,
            true,
            true,
        ),
        (
            json!({"bedrock_api_key":{"api_key":"key"}}),
            false,
            false,
            true,
        ),
        (
            json!({"bedrock_api_key":{"api_key":"key","region":"region"}}),
            false,
            true,
            true,
        ),
        (json!({"future_auth":{}}), false, false, false),
        (json!({"future_auth":false}), false, true, true),
        (
            json!({"OPENAI_API_KEY":"test-key", "last_refresh":"2026-09-05"}),
            true,
            false,
            true,
        ),
    ] {
        let observation = observe_auth(&auth);
        assert_eq!(observation.has_provider_api_key(), key, "{auth}");
        assert_eq!(observation.has_credential_material(), credentials, "{auth}");
        assert_eq!(observation.has_non_key_payload(), other_payload, "{auth}");
        assert_eq!(observation.has_payload(), key || other_payload, "{auth}");
        assert_eq!(auth_has_login_material(&auth), key || credentials, "{auth}");
        assert_eq!(
            auth_has_credential_login_material(&auth),
            credentials,
            "{auth}"
        );
    }
}

#[test]
fn auth_observations_do_not_retain_credential_values() {
    let observation = observe_auth(&json!({"OPENAI_API_KEY":"secret-api-key",
        "tokens":{"access_token":"secret-access-token"}, "future_secret":"secret-value"}));
    let debug = format!("{observation:?}");
    for secret in [
        "secret-api-key",
        "secret-access-token",
        "secret-value",
        "future_secret",
    ] {
        assert!(!debug.contains(secret));
    }
}
