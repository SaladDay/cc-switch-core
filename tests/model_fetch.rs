use cc_switch_core::model_fetch::{
    ModelEndpointInput::{BaseUrl, CompletionUrl},
    ModelEndpointPolicy, ModelFetchSpec, ModelHeaderValue, ModelListShape, ANTHROPIC_COMPATIBLE,
    BEARER_COMPATIBLE, GOOGLE_API_KEY,
};
use serde_json::json;

#[test]
fn registered_defaults_are_explicit_without_changing_descriptor_payloads() {
    let expected = [
        ("claude", Some(&ANTHROPIC_COMPATIBLE)),
        ("claude-desktop", None),
        ("codex", Some(&BEARER_COMPATIBLE)),
        ("gemini", Some(&GOOGLE_API_KEY)),
        ("grokbuild", None),
        ("opencode", Some(&BEARER_COMPATIBLE)),
        ("openclaw", Some(&BEARER_COMPATIBLE)),
        ("hermes", Some(&BEARER_COMPATIBLE)),
        ("pi", Some(&BEARER_COMPATIBLE)),
    ];
    let registry = cc_switch_core::builtin_app_registry();
    assert_eq!(registry.descriptors().len(), expected.len());
    for (descriptor, (id, spec)) in registry.descriptors().zip(expected) {
        assert_eq!(descriptor.id(), id);
        assert_eq!(descriptor.default_model_fetch_spec(), spec, "{id}");
        let serialized = serde_json::to_value(descriptor).unwrap();
        assert_eq!(serialized.as_object().unwrap().len(), 5, "{id}");
        for key in [
            "id",
            "displayName",
            "brandKey",
            "configurationMode",
            "capabilities",
        ] {
            assert!(serialized.get(key).is_some(), "{id}: {key}");
        }
    }
}

#[test]
fn compatible_specs_keep_candidate_and_header_order() {
    assert_eq!(
        BEARER_COMPATIBLE.candidate_urls(" https://relay.example/ ", BaseUrl),
        [
            "https://relay.example/models",
            "https://relay.example/v1/models",
        ]
    );
    assert_eq!(
        ANTHROPIC_COMPATIBLE.candidate_urls("https://relay.example/API/Anthropic", BaseUrl),
        [
            "https://relay.example/API/Anthropic/v1/models",
            "https://relay.example/v1/models",
            "https://relay.example/models",
        ]
    );
    assert_eq!(
        GOOGLE_API_KEY.candidate_urls("https://relay.example/v1beta", BaseUrl),
        ["https://relay.example/v1beta/models"]
    );
    assert_eq!(
        ANTHROPIC_COMPATIBLE
            .headers_for_key("key")
            .collect::<Vec<_>>(),
        [
            ("Authorization", "Bearer key".into()),
            ("x-api-key", "key".into()),
            ("anthropic-version", "2023-06-01".into()),
        ]
    );
    assert_eq!(
        GOOGLE_API_KEY.headers_for_key(" key ").collect::<Vec<_>>(),
        [("x-goog-api-key", " key ".into())]
    );
    assert_eq!(
        BEARER_COMPATIBLE.headers_for_key("").collect::<Vec<_>>(),
        [("Authorization", "Bearer ".into())]
    );
}

#[test]
fn endpoint_input_keeps_textual_derivation_without_validation() {
    for spec in [BEARER_COMPATIBLE, ANTHROPIC_COMPATIBLE, GOOGLE_API_KEY] {
        assert!(spec.candidate_urls(" \t/", BaseUrl).is_empty());
        assert!(spec
            .candidate_urls("https://relay.example", CompletionUrl)
            .is_empty());
        assert_eq!(
            spec.candidate_urls(
                "https://relay.example/v1/chat/completions?version=1",
                CompletionUrl
            ),
            ["https://relay.example/v1/models"]
        );
        assert_eq!(
            spec.candidate_urls("https://relay.example/custom/messages", CompletionUrl),
            ["https://relay.example/custom/v1/models"]
        );
        assert_eq!(
            spec.candidate_urls("https://relay.example/v1/models/", BaseUrl),
            ["https://relay.example/v1/models"]
        );
        assert_eq!(
            spec.candidate_urls("/v1/not-a-url", CompletionUrl),
            ["/v1/models"]
        );
    }
}

#[test]
fn response_alternatives_preserve_ids_and_leave_rich_metadata_available() {
    let response = json!({"data": [
        {"id": "", "capabilities": {"future": true}}, {"id": null}, {"id": " a "}, {"id": ""}, {"id": "雪"}
    ], "models": [{"name": "models/not-selected"}], "has_more": true, "last_id": "opaque-cursor"});
    let original = response.clone();
    for spec in [BEARER_COMPATIBLE, ANTHROPIC_COMPATIBLE, GOOGLE_API_KEY] {
        assert_eq!(spec.parse_model_ids(&response), ["", " a ", "雪"]);
        assert_eq!(
            spec.parse_model_ids(&json!({"data": [{"id": 7}], "models": [
                {"name": "models/models/a"}, {"name": "models/"}, {"name": "models/models/a"}
            ]})),
            ["models/a", ""]
        );
        assert_eq!(
            spec.parse_model_ids(&json!([{ "id": "a" }, null, {"id": "b"}, {"id": "a"}])),
            ["a", "b"]
        );
        for payload in [
            json!(null),
            json!([]),
            json!({"data": "bad"}),
            json!({"data": [false, {}, {"id": []}]}),
        ] {
            assert!(spec.parse_model_ids(&payload).is_empty());
        }
    }
    assert_eq!(response, original);
}

#[test]
fn a_rich_consumer_can_declare_rules_without_app_or_product_switches() {
    let spec = ModelFetchSpec {
        endpoints: ModelEndpointPolicy::VersionedFirst {
            compatibility_suffixes: &["/Native"],
        },
        key_headers: &[
            ("x-session", ModelHeaderValue::Key { prefix: "Session " }),
            ("x-version", ModelHeaderValue::Literal("2")),
        ],
        response_shapes: &[
            ModelListShape {
                collection_pointer: "/result/a~1b",
                id_pointer: "/native/id",
                strip_prefix: Some("vendor/"),
            },
            ModelListShape {
                collection_pointer: "",
                id_pointer: "/id",
                strip_prefix: None,
            },
        ],
    };
    assert_eq!(
        spec.candidate_urls("https://relay.example/nAtIvE", BaseUrl),
        [
            "https://relay.example/nAtIvE/v1/models",
            "https://relay.example/v1/models",
            "https://relay.example/models"
        ]
    );
    assert_eq!(
        spec.headers_for_key("opaque").collect::<Vec<_>>(),
        [
            ("x-session", "Session opaque".into()),
            ("x-version", "2".into())
        ]
    );
    let response = json!({"result": {"a/b": [{"native": {"id": "vendor/a"}}, {"native": {"id": "vendor/a"}}]}});
    assert_eq!(spec.parse_model_ids(&response), ["a"]);
    assert!(!format!("{spec:?}").contains("opaque"));
}
