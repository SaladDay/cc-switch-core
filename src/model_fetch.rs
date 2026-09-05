//! Declarative model-list request rules and response decoding.
//!
//! These rules do not execute HTTP requests, acquire credentials, validate URLs
//! or headers, or follow pagination. Hosts own those decisions and error handling.

use std::collections::HashSet;

use serde_json::Value;

/// What the supplied URL represents. Completion URLs use the existing
/// `/v1/` or parent-path derivation, independently of endpoint preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelEndpointInput {
    BaseUrl,
    CompletionUrl,
}

/// Candidate ordering for compatible model-list endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelEndpointPolicy {
    ModelsFirst,
    /// Prefer a versioned endpoint, then try the first matching compatibility
    /// suffix's root. Suffix matching is ASCII-case-insensitive and ordered.
    VersionedFirst {
        compatibility_suffixes: &'static [&'static str],
    },
}

/// One ordered header value rule, without an acquired credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelHeaderValue {
    Key { prefix: &'static str },
    Literal(&'static str),
}

/// One response alternative. Pointers use [`Value::pointer`]; an empty collection
/// pointer selects the root array. Missing collections and non-string IDs are skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelListShape {
    pub collection_pointer: &'static str,
    pub id_pointer: &'static str,
    pub strip_prefix: Option<&'static str>,
}

/// Static protocol rules, independent of an App or a product's form fields.
/// Definitions must not embed credentials; pass an acquired key only when
/// expanding headers. Header order and duplicate names are retained for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFetchSpec {
    pub endpoints: ModelEndpointPolicy,
    pub key_headers: &'static [(&'static str, ModelHeaderValue)],
    pub response_shapes: &'static [ModelListShape],
}

/// Interoperability response order used by existing consumers. The first shape
/// yielding any string IDs wins, including empty strings. No IDs are trimmed.
pub const COMPATIBLE_MODEL_LISTS: &[ModelListShape] = &[
    ModelListShape {
        collection_pointer: "/data",
        id_pointer: "/id",
        strip_prefix: None,
    },
    ModelListShape {
        collection_pointer: "/models",
        id_pointer: "/name",
        strip_prefix: Some("models/"),
    },
    ModelListShape {
        collection_pointer: "",
        id_pointer: "/id",
        strip_prefix: None,
    },
];

pub const BEARER_COMPATIBLE: ModelFetchSpec = ModelFetchSpec {
    endpoints: ModelEndpointPolicy::ModelsFirst,
    key_headers: &[("Authorization", ModelHeaderValue::Key { prefix: "Bearer " })],
    response_shapes: COMPATIBLE_MODEL_LISTS,
};

/// Existing compatible-provider behavior, including dual authentication headers
/// and compatibility-root fallbacks. This is not the canonical Anthropic default.
pub const ANTHROPIC_COMPATIBLE: ModelFetchSpec = ModelFetchSpec {
    endpoints: ModelEndpointPolicy::VersionedFirst {
        compatibility_suffixes: &[
            "/api/claudecode",
            "/api/anthropic",
            "/apps/anthropic",
            "/api/coding",
            "/claudecode",
            "/anthropic",
            "/step_plan",
            "/coding",
            "/claude",
        ],
    },
    key_headers: &[
        ("Authorization", ModelHeaderValue::Key { prefix: "Bearer " }),
        ("x-api-key", ModelHeaderValue::Key { prefix: "" }),
        ("anthropic-version", ModelHeaderValue::Literal("2023-06-01")),
    ],
    response_shapes: COMPATIBLE_MODEL_LISTS,
};

pub const GOOGLE_API_KEY: ModelFetchSpec = ModelFetchSpec {
    endpoints: ModelEndpointPolicy::ModelsFirst,
    key_headers: &[("x-goog-api-key", ModelHeaderValue::Key { prefix: "" })],
    response_shapes: COMPATIBLE_MODEL_LISTS,
};

impl ModelFetchSpec {
    /// Derives ordered, deduplicated candidates without validating or authorizing
    /// their use. Trims outer whitespace and trailing slashes, but otherwise keeps
    /// the consumer's textual URL rules, including query and malformed inputs.
    pub fn candidate_urls(&self, base_url: &str, input: ModelEndpointInput) -> Vec<String> {
        let base = base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Vec::new();
        }
        if input == ModelEndpointInput::CompletionUrl {
            if let Some(index) = base.find("/v1/") {
                return vec![format!("{}/v1/models", &base[..index])];
            }
            if let Some(index) = base.rfind('/') {
                let root = &base[..index];
                if root
                    .find("://")
                    .is_some_and(|scheme| root.len() > scheme.saturating_add(3))
                {
                    return vec![format!("{root}/v1/models")];
                }
            }
            return Vec::new();
        }
        if base.ends_with("/models") {
            return vec![base.to_string()];
        }
        let append_models = format!("{base}/models");
        let versioned = if base.ends_with("/v1") || base.ends_with("/v1beta") {
            None
        } else {
            Some(format!("{base}/v1/models"))
        };
        let mut urls = Vec::new();
        match self.endpoints {
            ModelEndpointPolicy::ModelsFirst => {
                urls.push(append_models);
                urls.extend(versioned);
            }
            ModelEndpointPolicy::VersionedFirst {
                compatibility_suffixes,
            } => {
                urls.push(versioned.as_ref().unwrap_or(&append_models).clone());
                let lower = base.to_ascii_lowercase();
                let stripped = compatibility_suffixes.iter().find_map(|suffix| {
                    lower
                        .ends_with(&suffix.to_ascii_lowercase())
                        .then(|| &base[..base.len() - suffix.len()])
                });
                if let Some(root) = stripped {
                    let root = root.trim_end_matches('/');
                    if !root.is_empty() && root.contains("://") {
                        urls.push(format!("{root}/v1/models"));
                        urls.push(format!("{root}/models"));
                    }
                } else if versioned.is_some() {
                    urls.push(append_models);
                }
            }
        }
        let mut seen = HashSet::new();
        urls.retain(|url| seen.insert(url.clone()));
        urls
    }

    /// Expands headers only for a host-supplied key. No trimming or validation is
    /// performed; hosts retain key presence rules and HTTP header diagnostics.
    /// Literal entries here accompany a key, not every unauthenticated request.
    pub fn headers_for_key<'a>(
        &'a self,
        key: &'a str,
    ) -> impl Iterator<Item = (&'static str, String)> + 'a {
        self.key_headers.iter().map(move |(name, value)| {
            let value = match value {
                ModelHeaderValue::Key { prefix } => format!("{prefix}{key}"),
                ModelHeaderValue::Literal(value) => (*value).to_owned(),
            };
            (*name, value)
        })
    }

    /// Extracts IDs from the first matching response alternative, deduplicating
    /// in arrival order. This does not mutate the response or consume metadata
    /// and pagination; hosts can retain the original JSON for richer workflows.
    ///
    /// ```
    /// use cc_switch_core::model_fetch::BEARER_COMPATIBLE;
    /// use serde_json::json;
    /// let response = json!({"data": [{"id": "a"}, {"id": "b"}, {"id": "a"}]});
    /// assert_eq!(BEARER_COMPATIBLE.parse_model_ids(&response), ["a", "b"]);
    /// ```
    pub fn parse_model_ids(&self, payload: &Value) -> Vec<String> {
        for shape in self.response_shapes {
            let Some(items) = payload
                .pointer(shape.collection_pointer)
                .and_then(Value::as_array)
            else {
                continue;
            };
            let mut seen = HashSet::new();
            let ids: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    let id = item.pointer(shape.id_pointer)?.as_str()?;
                    let id = shape
                        .strip_prefix
                        .and_then(|prefix| id.strip_prefix(prefix))
                        .unwrap_or(id);
                    seen.insert(id).then(|| id.to_owned())
                })
                .collect();
            if !ids.is_empty() {
                return ids;
            }
        }
        Vec::new()
    }
}
