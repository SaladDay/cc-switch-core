//! Claude Desktop live-profile projection.

use std::fmt;

use serde_json::{json, Value};
use thiserror::Error;

use crate::{
    integration::AppIntegration,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, CLAUDE_DESKTOP_FORM},
    AppType, LogicalTarget,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::ClaudeDesktop,
        "claude-desktop",
        "Claude Desktop",
        "claude",
        ProviderConfigurationMode::Switch,
        CAPABILITIES,
        &["claude_desktop", "claudedesktop"],
    ),
    &[
        LogicalTarget::ClaudeDesktopNormalConfig,
        LogicalTarget::ClaudeDesktopThreepConfig,
        LogicalTarget::ClaudeDesktopProfile,
        LogicalTarget::ClaudeDesktopMeta,
    ],
    &CLAUDE_DESKTOP_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_claude_like,
        simple_provider::project_claude_desktop,
        false,
    ),
    NativeImportBehavior::new(native_import::import_claude_desktop),
    NativeProjectionBehavior::new(
        projection::claude_desktop_plan,
        None,
        projection::declared_native_targets,
        NativeContextRequirement::ClaudeDesktop,
    ),
);

const ONE_M_CONTEXT_MARKER: &str = "[1m]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    Official,
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectModelRoute {
    pub route_id: String,
    pub upstream_model: String,
    pub label_override: Option<String>,
    pub supports_1m: bool,
}

#[derive(Clone, PartialEq)]
pub enum PreparedLiveAction {
    RestoreOfficial,
    ApplyDirect { profile: Value },
}

impl fmt::Debug for PreparedLiveAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RestoreOfficial => formatter.write_str("RestoreOfficial"),
            Self::ApplyDirect { .. } => formatter
                .debug_struct("ApplyDirect")
                .field("profile", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareLiveActionError {
    #[error("Claude Desktop direct settings must be a JSON object")]
    SettingsNotObject,
    #[error("Claude Desktop direct settings are missing the 'env' object")]
    MissingEnv,
    #[error("Claude Desktop direct settings are missing ANTHROPIC_BASE_URL")]
    MissingBaseUrl,
    #[error("Claude Desktop direct settings are missing ANTHROPIC_AUTH_TOKEN")]
    MissingAuthToken,
    #[error("Claude Desktop direct model route is invalid")]
    InvalidModelRoute,
    #[error("Claude Desktop direct mode cannot remap an upstream model")]
    UnsupportedModelMapping,
}

/// Builds the write intent for Claude Desktop without touching platform files.
///
/// Missing or empty routes omit `inferenceModels`, matching the direct native
/// writer. Proxy-mode credentials and routes deliberately remain in consumer
/// applications.
pub fn prepare_live_action(
    settings: &Value,
    mode: ProviderMode,
    routes: Option<&[DirectModelRoute]>,
) -> Result<PreparedLiveAction, PrepareLiveActionError> {
    if mode == ProviderMode::Official {
        return Ok(PreparedLiveAction::RestoreOfficial);
    }

    let settings = settings
        .as_object()
        .ok_or(PrepareLiveActionError::SettingsNotObject)?;
    let env = settings
        .get("env")
        .and_then(Value::as_object)
        .ok_or(PrepareLiveActionError::MissingEnv)?;
    let base_url =
        required_env(env, "ANTHROPIC_BASE_URL").ok_or(PrepareLiveActionError::MissingBaseUrl)?;
    let api_key = required_env(env, "ANTHROPIC_AUTH_TOKEN")
        .ok_or(PrepareLiveActionError::MissingAuthToken)?;

    let mut profile = json!({
        "coworkEgressAllowedHosts": ["*"],
        "disableDeploymentModeChooser": true,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceProvider": "gateway"
    });
    if let Some(routes) = routes.filter(|routes| !routes.is_empty()) {
        profile["inferenceModels"] = Value::Array(prepare_routes(routes)?);
    }
    Ok(PreparedLiveAction::ApplyDirect { profile })
}

fn required_env<'a>(env: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    env.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn prepare_routes(routes: &[DirectModelRoute]) -> Result<Vec<Value>, PrepareLiveActionError> {
    let mut routes = routes
        .iter()
        .filter_map(|route| {
            let route_id = route.route_id.trim();
            (!route_id.is_empty()).then_some((route_id, route))
        })
        .map(|(route_id, route)| {
            if !is_claude_safe_model_id(route_id) {
                return Err(PrepareLiveActionError::InvalidModelRoute);
            }
            let upstream = route.upstream_model.trim();
            if !upstream.is_empty() && upstream != route_id {
                return Err(PrepareLiveActionError::UnsupportedModelMapping);
            }
            Ok((
                route_id.to_owned(),
                route
                    .label_override
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                route.supports_1m,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    routes.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| right.2.cmp(&left.2)));
    routes.dedup_by(|left, right| left.0 == right.0);
    Ok(routes
        .into_iter()
        .map(|(name, label_override, supports_1m)| {
            if !supports_1m && label_override.is_none() {
                return Value::String(name);
            }
            let mut model = json!({"name": name});
            if let Some(label) = label_override {
                model["labelOverride"] = Value::String(label);
            }
            if supports_1m {
                model["supports1m"] = Value::Bool(true);
            }
            model
        })
        .collect())
}

pub fn is_claude_safe_model_id(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.contains(ONE_M_CONTEXT_MARKER) {
        return false;
    }
    let Some(route_tail) = normalized
        .strip_prefix("anthropic/claude-")
        .or_else(|| normalized.strip_prefix("claude-"))
    else {
        return false;
    };
    ["sonnet-", "opus-", "haiku-", "fable-"]
        .iter()
        .any(|prefix| {
            route_tail
                .strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn official_mode_does_not_require_direct_credentials() {
        assert_eq!(
            prepare_live_action(&Value::Null, ProviderMode::Official, None),
            Ok(PreparedLiveAction::RestoreOfficial)
        );
    }

    #[test]
    fn empty_direct_routes_use_native_model_discovery() {
        let action = prepare_live_action(
            &json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://example.com",
                    "ANTHROPIC_AUTH_TOKEN": "secret"
                }
            }),
            ProviderMode::Direct,
            Some(&[]),
        )
        .expect("valid direct settings");

        let PreparedLiveAction::ApplyDirect { profile } = action else {
            panic!("expected direct profile");
        };
        assert!(profile.get("inferenceModels").is_none());
    }

    #[test]
    fn prepares_a_direct_gateway_profile_and_valid_routes() {
        let action = prepare_live_action(
            &json!({
                "env": {
                    "ANTHROPIC_BASE_URL": " https://example.com ",
                    "ANTHROPIC_AUTH_TOKEN": " secret "
                }
            }),
            ProviderMode::Direct,
            Some(&[
                DirectModelRoute {
                    route_id: "claude-sonnet-4-6".to_owned(),
                    upstream_model: "claude-sonnet-4-6".to_owned(),
                    label_override: None,
                    supports_1m: false,
                },
                DirectModelRoute {
                    route_id: "anthropic/claude-opus-4-6".to_owned(),
                    upstream_model: String::new(),
                    label_override: Some("Opus".to_owned()),
                    supports_1m: true,
                },
            ]),
        )
        .expect("valid direct settings");

        let PreparedLiveAction::ApplyDirect { profile } = action else {
            panic!("expected a direct action");
        };
        assert_eq!(profile["inferenceGatewayBaseUrl"], "https://example.com");
        assert_eq!(profile["inferenceGatewayApiKey"], "secret");
        assert_eq!(profile["inferenceModels"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn rejects_missing_credentials_and_unsafe_routes() {
        assert_eq!(
            prepare_live_action(&json!({"env": {}}), ProviderMode::Direct, None),
            Err(PrepareLiveActionError::MissingBaseUrl)
        );
        assert_eq!(
            prepare_live_action(
                &json!({
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://example.com",
                        "ANTHROPIC_AUTH_TOKEN": "secret"
                    }
                }),
                ProviderMode::Direct,
                Some(&[DirectModelRoute {
                    route_id: "gpt-5".to_owned(),
                    upstream_model: "gpt-5".to_owned(),
                    label_override: None,
                    supports_1m: false,
                }]),
            ),
            Err(PrepareLiveActionError::InvalidModelRoute)
        );
    }

    #[test]
    fn debug_output_redacts_the_direct_profile() {
        let action = prepare_live_action(
            &json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://example.com",
                    "ANTHROPIC_AUTH_TOKEN": "do-not-log"
                }
            }),
            ProviderMode::Direct,
            None,
        )
        .expect("valid settings");

        let debug = format!("{action:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("inferenceGatewayApiKey"));
    }
}
