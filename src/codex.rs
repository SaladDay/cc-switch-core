//! Codex live-configuration preparation.

use std::{collections::HashSet, error::Error, fmt};

use http::Uri;
use serde_json::{json, Value};
use thiserror::Error;
use toml_edit::DocumentMut;

use crate::{
    integration::AppIntegration,
    mcp::CODEX_MCP,
    native_import::{self, NativeImportBehavior},
    projection::{self, NativeContextRequirement, NativePolicyBehavior, NativeProjectionBehavior},
    registry::{AppCapability, AppDescriptor, ProviderConfigurationMode},
    simple_provider::{self, SimpleProviderBehavior, CODEX_FORM},
    AppType, LogicalTarget, NativeConfigRoot, NativeResourcePath, SkillAppContract, SkillDiscovery,
};

const CAPABILITIES: &[AppCapability] = &[
    AppCapability::ProviderManagement,
    AppCapability::LiveConfiguration,
    AppCapability::CommonConfiguration,
    AppCapability::LocalProxy,
    AppCapability::Mcp,
    AppCapability::Prompts,
    AppCapability::Skills,
];

pub(crate) const INTEGRATION: AppIntegration = AppIntegration::new(
    AppDescriptor::new(
        AppType::Codex,
        "codex",
        "Codex",
        "codex",
        ProviderConfigurationMode::Switch,
        CAPABILITIES,
        &[],
    )
    .with_config_root(NativeConfigRoot::home_relative(".codex"))
    .with_mcp(&CODEX_MCP)
    .with_skills(SkillAppContract::catalog(
        "enabled_codex",
        SkillDiscovery::NativeAndUnified,
        None,
        NativeResourcePath::relative("skills"),
    )),
    &[
        LogicalTarget::CodexAuth,
        LogicalTarget::CodexConfig,
        LogicalTarget::CodexModelCatalog,
    ],
    &CODEX_FORM,
    SimpleProviderBehavior::new(
        simple_provider::extract_codex,
        simple_provider::project_codex,
        false,
    ),
    NativeImportBehavior::new(native_import::import_codex),
    NativeProjectionBehavior::new(
        projection::codex_plan,
        None,
        projection::codex_native_targets,
        NativeContextRequirement::Standard,
    )
    .with_policy(NativePolicyBehavior::new(
        projection::codex_policy_plan,
        projection::codex_policy_targets,
    )),
);

pub const MODEL_CATALOG_FILENAME: &str = "cc-switch-model-catalog.json";

const RESERVED_MODEL_PROVIDER_IDS: &[&str] = &[
    "amazon-bedrock",
    "openai",
    "ollama",
    "lmstudio",
    "oss",
    "ollama-chat",
];
const WEB_SEARCH_REJECT_HOSTS: &[&str] = &[
    "xiaomimimo.com",
    "longcat.chat",
    "minimax.io",
    "minimaxi.com",
];
const WEB_SEARCH_REJECT_MODEL_PREFIXES: &[&str] = &["mimo", "longcat", "minimax", "qwen3-coder"];
const DEEPSEEK_OFFICIAL_CATALOG_HOSTS: &[&str] = &["deepseek.com"];
const CONFIRMED_TEXT_ONLY_MODELS: &[&str] = &[
    "ark-code-latest",
    "deepseek-chat",
    "deepseek-reasoner",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "glm-5.1",
    "glm-5.2",
    "kat-coder",
    "kat-coder-pro",
    "kat-coder-pro v1",
    "kat-coder-pro v2",
    "kat-coder-pro-v1",
    "kat-coder-pro-v2",
    "ling-2.5-1t",
    "longcat-2.0",
    "longcat-flash-chat",
    "minimax-m2.7",
    "minimax-m2.7-highspeed",
    "mimo-v2.5-pro",
    "qwen3-coder-480b",
    "qwen3-coder-480b-a35b-instruct",
    "qwen3-coder-flash",
    "qwen3-coder-next",
    "qwen3-coder-plus",
    "step-3.5-flash",
    "step-3.5-flash-2603",
    "us.deepseek.r1-v1",
];

/// Owned values required by the Codex live-write pipeline.
#[derive(Clone, PartialEq)]
pub struct PreparedLiveSnapshot {
    pub auth: Value,
    pub config: Option<String>,
}

impl fmt::Debug for PreparedLiveSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLiveSnapshot")
            .field("auth", &"<redacted>")
            .field("config", &"<redacted>")
            .finish()
    }
}

/// Validation errors returned while preparing a Codex live snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareLiveSnapshotError {
    SettingsNotObject,
    MissingAuth,
}

impl fmt::Display for PrepareLiveSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SettingsNotObject => "Codex settings must be a JSON object",
            Self::MissingAuth => "Codex settings are missing the 'auth' field",
        })
    }
}

impl Error for PrepareLiveSnapshotError {}

/// Extracts the values consumed by the Codex live-write pipeline.
#[deprecated(note = "use prepare_strict_live_snapshot for shared live writers")]
pub fn prepare_live_snapshot(
    settings: &Value,
) -> Result<PreparedLiveSnapshot, PrepareLiveSnapshotError> {
    let object = settings
        .as_object()
        .ok_or(PrepareLiveSnapshotError::SettingsNotObject)?;
    let auth = object
        .get("auth")
        .ok_or(PrepareLiveSnapshotError::MissingAuth)?;
    Ok(PreparedLiveSnapshot {
        auth: auth.clone(),
        config: object
            .get("config")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Errors returned by the strict Codex projection used by new live writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareStrictLiveSnapshotError {
    SettingsNotObject,
    MissingAuth,
    AuthNotObject,
    ConfigNotString,
    InvalidConfig,
}

impl fmt::Display for PrepareStrictLiveSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SettingsNotObject => "Codex settings must be a JSON object",
            Self::MissingAuth => "Codex settings are missing the 'auth' field",
            Self::AuthNotObject => "Codex 'auth' must be a JSON object",
            Self::ConfigNotString => "Codex 'config' must be a TOML string",
            Self::InvalidConfig => "Codex 'config' must be valid TOML",
        })
    }
}

impl Error for PrepareStrictLiveSnapshotError {}

/// Validates the native Codex shapes before preparing a live snapshot.
pub fn prepare_strict_live_snapshot(
    settings: &Value,
) -> Result<PreparedLiveSnapshot, PrepareStrictLiveSnapshotError> {
    let object = settings
        .as_object()
        .ok_or(PrepareStrictLiveSnapshotError::SettingsNotObject)?;
    let auth = object
        .get("auth")
        .ok_or(PrepareStrictLiveSnapshotError::MissingAuth)?;
    if !auth.is_object() {
        return Err(PrepareStrictLiveSnapshotError::AuthNotObject);
    }
    let config = match object.get("config") {
        Some(Value::String(config)) => {
            config
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| PrepareStrictLiveSnapshotError::InvalidConfig)?;
            Some(config.clone())
        }
        Some(Value::Null) | None => None,
        Some(_) => return Err(PrepareStrictLiveSnapshotError::ConfigNotString),
    };

    Ok(PreparedLiveSnapshot {
        auth: auth.clone(),
        config,
    })
}

#[derive(Clone, PartialEq)]
pub struct PreparedNativeCatalog {
    pub config: String,
    pub catalog: Option<Value>,
    /// False means the provider did not declare `modelCatalog`, so callers
    /// must leave an existing catalog file alone.
    pub managed: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeCatalogOwnership {
    /// True only when the caller has durable evidence that CC Switch wrote the
    /// current `web_search = "disabled"` sentinel.
    pub web_search_disabled: bool,
}

impl fmt::Debug for PreparedNativeCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedNativeCatalog")
            .field("config", &"<redacted>")
            .field("catalog", &"<redacted>")
            .field("managed", &self.managed)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareNativeLiveError {
    #[error("Codex third-party auth requires a config.toml projection")]
    MissingConfigForApiKey,
    #[error("Codex config must be valid TOML")]
    InvalidConfig,
    #[error("Codex modelCatalog must be a JSON object")]
    ModelCatalogNotObject,
    #[error("Codex modelCatalog.models must be an array")]
    ModelsNotArray,
    #[error("every Codex modelCatalog row must contain a non-empty string model id")]
    InvalidCatalogModel,
    #[error("bundled Codex model catalog template is invalid")]
    InvalidCatalogTemplate,
    #[error("Codex web_search is user-managed and conflicts with the required gateway setting")]
    WebSearchConflict,
}

/// Whether an auth object carries material that can authenticate Codex.
pub fn auth_has_login_material(auth: &Value) -> bool {
    let Some(object) = auth.as_object() else {
        return false;
    };
    object
        .iter()
        .any(|(key, value)| auth_field_has_login_material(key, value, true))
}

/// Returns true for first-class login credentials, excluding a provider API
/// key and inert metadata.
pub fn auth_has_credential_login_material(auth: &Value) -> bool {
    let Some(object) = auth.as_object() else {
        return false;
    };
    object
        .iter()
        .any(|(key, value)| auth_field_has_login_material(key, value, false))
}

fn auth_field_has_login_material(key: &str, value: &Value, include_provider_api_key: bool) -> bool {
    match key {
        "auth_mode" | "last_refresh" => false,
        "OPENAI_API_KEY" => include_provider_api_key && nonempty_string(value),
        "tokens" => token_map_has_login_material(value),
        "personal_access_token" => nonempty_string(value),
        "agent_identity" => agent_identity_has_login_material(value),
        "bedrock_api_key" => bedrock_api_key_has_login_material(value),
        // Unknown non-empty fields may be new official credential carriers.
        // Both predicates preserve them so cleanup cannot destroy future auth.
        _ => value_is_present(value),
    }
}

fn nonempty_string(value: &Value) -> bool {
    value
        .as_str()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

fn token_map_has_login_material(value: &Value) -> bool {
    value.as_object().is_some_and(|tokens| {
        ["id_token", "access_token", "refresh_token"]
            .iter()
            .any(|key| tokens.get(*key).is_some_and(nonempty_string))
    })
}

fn agent_identity_has_login_material(value: &Value) -> bool {
    if nonempty_string(value) {
        return true;
    }
    value.as_object().is_some_and(|identity| {
        ["agent_runtime_id", "agent_private_key"]
            .iter()
            .all(|key| identity.get(*key).is_some_and(nonempty_string))
    })
}

fn bedrock_api_key_has_login_material(value: &Value) -> bool {
    value.as_object().is_some_and(|auth| {
        ["api_key", "region"]
            .iter()
            .all(|key| auth.get(*key).is_some_and(nonempty_string))
    })
}

/// Identifies the residue produced by an API-key switch without mistaking an
/// OAuth cache for disposable state.
pub fn live_auth_is_stale_third_party_residue(auth: &Value) -> bool {
    !auth_has_credential_login_material(auth)
        && auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|key| !key.is_empty())
}

/// Shared auth routing policy. Official providers write auth only when they
/// carry login material. Third-party providers obey the caller's preservation
/// setting and can instead project their key into config.toml.
pub fn should_write_auth(
    category: Option<&str>,
    auth: &Value,
    preserve_login_cache_for_third_party: bool,
) -> bool {
    (category == Some("official") && auth_has_login_material(auth))
        || (category != Some("official") && !preserve_login_cache_for_third_party)
}

/// Projects a stored `OPENAI_API_KEY` into the active provider's
/// `experimental_bearer_token`, allowing a config-only switch to preserve the
/// user's long-lived ChatGPT login cache.
pub fn prepare_provider_live_config(
    auth: &Value,
    config: &str,
) -> Result<String, PrepareNativeLiveError> {
    let token = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| extract_experimental_bearer_token(config));
    let Some(token) = token else {
        return Ok(config.to_owned());
    };
    if config.trim().is_empty() {
        return Err(PrepareNativeLiveError::MissingConfigForApiKey);
    }
    let mut document = config
        .parse::<DocumentMut>()
        .map_err(|_| PrepareNativeLiveError::InvalidConfig)?;
    let provider_id = active_provider_id(&document);
    if let Some(provider_id) = provider_id.filter(|id| is_custom_provider_id(id)) {
        if let Some(provider) = document
            .get_mut("model_providers")
            .and_then(|item| item.as_table_mut())
            .and_then(|providers| providers.get_mut(&provider_id))
            .and_then(|item| item.as_table_mut())
        {
            provider["experimental_bearer_token"] = toml_edit::value(&token);
            return Ok(document.to_string());
        }
    }
    document["experimental_bearer_token"] = toml_edit::value(&token);
    Ok(document.to_string())
}

/// Generates Lite's direct/native Codex model catalog and maintains only the
/// cc-switch-owned pointer and web-search sentinel in config.toml.
pub fn prepare_native_model_catalog(
    settings: &Value,
    config: &str,
    ownership: NativeCatalogOwnership,
) -> Result<PreparedNativeCatalog, PrepareNativeLiveError> {
    let Some(catalog_settings) = settings.get("modelCatalog") else {
        return Ok(PreparedNativeCatalog {
            config: config.to_owned(),
            catalog: None,
            managed: false,
        });
    };
    let catalog = catalog_settings
        .as_object()
        .ok_or(PrepareNativeLiveError::ModelCatalogNotObject)?;
    let models = catalog
        .get("models")
        .and_then(Value::as_array)
        .ok_or(PrepareNativeLiveError::ModelsNotArray)?;
    let specs = catalog_specs(models)?;
    let mut document = if config.trim().is_empty() {
        DocumentMut::new()
    } else {
        config
            .parse::<DocumentMut>()
            .map_err(|_| PrepareNativeLiveError::InvalidConfig)?
    };

    if specs.is_empty() {
        remove_owned_catalog_pointer(&mut document);
        if ownership.web_search_disabled {
            remove_owned_web_search_sentinel(&mut document);
        }
        return Ok(PreparedNativeCatalog {
            config: document.to_string(),
            catalog: None,
            managed: true,
        });
    }

    set_owned_catalog_pointer(&mut document);
    if native_gateway_rejects_web_search(&document.to_string()) {
        match document.get("web_search") {
            None => document["web_search"] = toml_edit::value("disabled"),
            Some(item) if item.as_str() == Some("disabled") => {
                document["web_search"] = toml_edit::value("disabled");
            }
            Some(_) => return Err(PrepareNativeLiveError::WebSearchConflict),
        }
    } else if ownership.web_search_disabled {
        remove_owned_web_search_sentinel(&mut document);
    }
    let default_context_window = document
        .get("model_context_window")
        .and_then(|item| item.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(128_000);
    let entries: Vec<Value> = if let Some(vendor_models) = official_vendor_catalog_models(&document)
    {
        specs
            .iter()
            .enumerate()
            .map(|(index, spec)| vendor_catalog_entry(&vendor_models, spec, index))
            .collect()
    } else {
        let template: Value = serde_json::from_str(include_str!(
            "resources/codex_native_responses_template.json"
        ))
        .map_err(|_| PrepareNativeLiveError::InvalidCatalogTemplate)?;
        specs
            .iter()
            .enumerate()
            .map(|(index, spec)| catalog_entry(&template, spec, index, default_context_window))
            .collect()
    };
    Ok(PreparedNativeCatalog {
        config: document.to_string(),
        catalog: Some(json!({"models": entries})),
        managed: true,
    })
}

fn value_is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        _ => true,
    }
}

fn active_provider_id(document: &DocumentMut) -> Option<String> {
    document
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn extract_experimental_bearer_token(config: &str) -> Option<String> {
    if !config.contains("experimental_bearer_token") {
        return None;
    }
    let document = config.parse::<DocumentMut>().ok()?;
    let top_level = || {
        document
            .get("experimental_bearer_token")
            .and_then(|item| item.as_str())
    };
    let token = match active_provider_id(&document) {
        Some(id) if is_custom_provider_id(&id) => document
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|providers| providers.get(&id))
            .and_then(|item| item.as_table_like())
            .and_then(|provider| provider.get("experimental_bearer_token"))
            .and_then(|item| item.as_str())
            .or_else(top_level),
        _ => top_level(),
    };
    token
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

fn is_custom_provider_id(id: &str) -> bool {
    !RESERVED_MODEL_PROVIDER_IDS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(id.trim()))
}

fn set_owned_catalog_pointer(document: &mut DocumentMut) {
    let owned_or_absent = document
        .get("model_catalog_json")
        .and_then(|item| item.as_str())
        .map(|path| path == MODEL_CATALOG_FILENAME)
        .unwrap_or(true);
    if owned_or_absent {
        document["model_catalog_json"] = toml_edit::value(MODEL_CATALOG_FILENAME);
    }
}

fn remove_owned_catalog_pointer(document: &mut DocumentMut) {
    let owned = document
        .get("model_catalog_json")
        .and_then(|item| item.as_str())
        .is_some_and(|path| path == MODEL_CATALOG_FILENAME);
    if owned {
        document.as_table_mut().remove("model_catalog_json");
    }
}

fn remove_owned_web_search_sentinel(document: &mut DocumentMut) {
    if document.get("web_search").and_then(|item| item.as_str()) == Some("disabled") {
        document.as_table_mut().remove("web_search");
    }
}

fn native_gateway_rejects_web_search(config: &str) -> bool {
    let Ok(document) = config.parse::<DocumentMut>() else {
        return false;
    };
    let provider_id = document
        .get("model_provider")
        .and_then(|item| item.as_str());
    let provider_base_url = provider_id.and_then(|id| {
        document
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|providers| providers.get(id))
            .and_then(|item| item.as_table_like())
            .and_then(|provider| provider.get("base_url"))
            .and_then(|item| item.as_str())
    });
    let base_url =
        provider_base_url.or_else(|| document.get("base_url").and_then(|item| item.as_str()));
    if base_url.is_some_and(|url| endpoint_matches_any_domain(url, WEB_SEARCH_REJECT_HOSTS)) {
        return true;
    }
    document
        .get("model")
        .and_then(|item| item.as_str())
        .map(str::to_ascii_lowercase)
        .map(|model| model.rsplit('/').next().unwrap_or(&model).to_owned())
        .is_some_and(|model| {
            WEB_SEARCH_REJECT_MODEL_PREFIXES
                .iter()
                .any(|prefix| model.starts_with(prefix))
        })
}

fn official_vendor_catalog_models(document: &DocumentMut) -> Option<Vec<Value>> {
    let provider_id = document
        .get("model_provider")
        .and_then(|item| item.as_str());
    let provider_base_url = provider_id.and_then(|id| {
        document
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|providers| providers.get(id))
            .and_then(|item| item.as_table_like())
            .and_then(|provider| provider.get("base_url"))
            .and_then(|item| item.as_str())
    });
    let base_url =
        provider_base_url.or_else(|| document.get("base_url").and_then(|item| item.as_str()))?;
    if !endpoint_matches_any_domain(base_url, DEEPSEEK_OFFICIAL_CATALOG_HOSTS) {
        return None;
    }
    let catalog: Value = serde_json::from_str(include_str!(
        "resources/codex_deepseek_catalog_template.json"
    ))
    .ok()?;
    catalog.get("models")?.as_array().cloned()
}

fn endpoint_matches_any_domain(endpoint: &str, domains: &[&str]) -> bool {
    let Ok(uri) = endpoint.trim().parse::<Uri>() else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return false;
    }
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.is_empty()
        && domains.iter().any(|domain| {
            let domain = domain.to_ascii_lowercase();
            host == domain || host.ends_with(&format!(".{domain}"))
        })
}

fn vendor_catalog_entry(vendor_models: &[Value], spec: &CatalogSpec, priority: usize) -> Value {
    let matched = vendor_models.iter().find(|entry| {
        entry
            .get("slug")
            .and_then(Value::as_str)
            .is_some_and(|slug| slug.eq_ignore_ascii_case(&spec.model))
    });
    let mut entry = matched
        .cloned()
        .or_else(|| vendor_models.first().cloned())
        .unwrap_or_else(|| json!({}));
    let Some(object) = entry.as_object_mut() else {
        return json!({});
    };
    if matched.is_none() {
        let display_name = spec.display_name.as_deref().unwrap_or(&spec.model);
        object.insert("slug".to_owned(), json!(spec.model));
        object.insert("display_name".to_owned(), json!(display_name));
        object.insert("description".to_owned(), json!(display_name));
        object.insert("priority".to_owned(), json!(1000 + priority));
    }
    if let Some(display_name) = &spec.display_name {
        object.insert("display_name".to_owned(), json!(display_name));
    }
    if let Some(context_window) = spec.context_window {
        object.insert("context_window".to_owned(), json!(context_window));
        object.insert("max_context_window".to_owned(), json!(context_window));
    }
    if let Some(parallel) = spec.parallel_tools {
        object.insert("supports_parallel_tool_calls".to_owned(), json!(parallel));
    }
    if let Some(modalities) = &spec.modalities {
        object.insert("input_modalities".to_owned(), json!(modalities));
    }
    if let Some(instructions) = &spec.base_instructions {
        object.insert("base_instructions".to_owned(), json!(instructions));
    }
    entry
}

#[derive(Debug)]
struct CatalogSpec {
    model: String,
    display_name: Option<String>,
    context_window: Option<u64>,
    parallel_tools: Option<bool>,
    modalities: Option<Vec<String>>,
    base_instructions: Option<String>,
}

fn catalog_specs(models: &[Value]) -> Result<Vec<CatalogSpec>, PrepareNativeLiveError> {
    let mut seen = HashSet::new();
    let mut specs = Vec::with_capacity(models.len());
    for model in models {
        let id = model
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(PrepareNativeLiveError::InvalidCatalogModel)?;
        if !seen.insert(id.to_owned()) {
            continue;
        }
        specs.push(CatalogSpec {
            model: id.to_owned(),
            display_name: string_alias(model, "displayName", "display_name"),
            context_window: positive_u64_alias(model, "contextWindow", "context_window"),
            parallel_tools: model
                .get("supportsParallelToolCalls")
                .or_else(|| model.get("supports_parallel_tool_calls"))
                .and_then(Value::as_bool),
            modalities: model
                .get("inputModalities")
                .or_else(|| model.get("input_modalities"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|items| !items.is_empty()),
            base_instructions: string_alias(model, "baseInstructions", "base_instructions"),
        });
    }
    Ok(specs)
}

fn string_alias(value: &Value, first: &str, second: &str) -> Option<String> {
    value
        .get(first)
        .or_else(|| value.get(second))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn positive_u64_alias(value: &Value, first: &str, second: &str) -> Option<u64> {
    match value.get(first).or_else(|| value.get(second)) {
        Some(Value::Number(value)) => value.as_u64().filter(|value| *value > 0),
        Some(Value::String(value)) => value.trim().parse().ok().filter(|value| *value > 0),
        _ => None,
    }
}

fn catalog_entry(
    template: &Value,
    spec: &CatalogSpec,
    index: usize,
    default_context_window: u64,
) -> Value {
    let mut entry = template.clone();
    let Some(object) = entry.as_object_mut() else {
        return json!({});
    };
    let display_name = spec.display_name.as_deref().unwrap_or(&spec.model);
    let context_window = spec.context_window.unwrap_or(default_context_window);
    object.insert("slug".to_owned(), json!(spec.model));
    object.insert("display_name".to_owned(), json!(display_name));
    object.insert("description".to_owned(), json!(display_name));
    object.insert("context_window".to_owned(), json!(context_window));
    object.insert("max_context_window".to_owned(), json!(context_window));
    object.insert("priority".to_owned(), json!(1000 + index));
    object.insert("additional_speed_tiers".to_owned(), json!([]));
    object.insert("service_tiers".to_owned(), json!([]));
    object.insert("availability_nux".to_owned(), Value::Null);
    object.insert("upgrade".to_owned(), Value::Null);
    object.insert(
        "input_modalities".to_owned(),
        json!(catalog_modalities(&spec.model, spec.modalities.as_deref())),
    );
    if let Some(parallel) = spec.parallel_tools {
        object.insert("supports_parallel_tool_calls".to_owned(), json!(parallel));
    }
    if let Some(instructions) = &spec.base_instructions {
        object.insert("base_instructions".to_owned(), json!(instructions));
    }
    entry
}

fn catalog_modalities(model: &str, declared: Option<&[String]>) -> Vec<&'static str> {
    let supports_image = declared
        .map(|items| {
            items
                .iter()
                .any(|item| item.trim().eq_ignore_ascii_case("image"))
        })
        .unwrap_or_else(|| {
            let model = model.trim().to_ascii_lowercase();
            let tail = model.rsplit('/').next().unwrap_or(&model);
            !CONFIRMED_TEXT_ONLY_MODELS.contains(&tail)
        });
    if supports_image {
        vec!["text", "image"]
    } else {
        vec!["text"]
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_owned_live_values_without_changing_the_input() {
        let settings = json!({
            "auth": {"OPENAI_API_KEY": "secret"},
            "config": "model = \"gpt-5\"",
            "metadata": {"preserved": true}
        });

        let snapshot = prepare_live_snapshot(&settings).expect("valid settings");

        assert_eq!(snapshot.auth, json!({"OPENAI_API_KEY": "secret"}));
        assert_eq!(snapshot.config.as_deref(), Some("model = \"gpt-5\""));
        assert_eq!(settings["metadata"], json!({"preserved": true}));
    }

    #[test]
    fn treats_absent_or_non_string_config_as_absent() {
        for settings in [json!({"auth": null}), json!({"auth": [], "config": 1})] {
            let snapshot = prepare_live_snapshot(&settings).expect("valid settings");

            assert_eq!(snapshot.config, None);
        }
    }

    #[test]
    fn rejects_non_object_settings() {
        assert_eq!(
            prepare_live_snapshot(&json!([])),
            Err(PrepareLiveSnapshotError::SettingsNotObject)
        );
    }

    #[test]
    fn rejects_settings_without_auth() {
        assert_eq!(
            prepare_live_snapshot(&json!({"config": "model = \"gpt-5\""})),
            Err(PrepareLiveSnapshotError::MissingAuth)
        );
    }

    #[test]
    fn strict_snapshot_rejects_invalid_auth_and_config_shapes() {
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": null})),
            Err(PrepareStrictLiveSnapshotError::AuthNotObject)
        );
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": {}, "config": 1})),
            Err(PrepareStrictLiveSnapshotError::ConfigNotString)
        );
        assert_eq!(
            prepare_strict_live_snapshot(&json!({"auth": {}, "config": "not = [toml"})),
            Err(PrepareStrictLiveSnapshotError::InvalidConfig)
        );
    }

    #[test]
    fn strict_snapshot_accepts_valid_native_values() {
        let snapshot = prepare_strict_live_snapshot(&json!({
            "auth": {"OPENAI_API_KEY": "secret"},
            "config": "model = \"gpt-5\""
        }))
        .expect("valid strict snapshot");

        assert_eq!(snapshot.config.as_deref(), Some("model = \"gpt-5\""));
    }

    #[test]
    fn debug_output_redacts_live_values() {
        let snapshot = prepare_live_snapshot(&json!({
            "auth": {"OPENAI_API_KEY": "do-not-log"},
            "config": "experimental_bearer_token = \"also-private\""
        }))
        .expect("valid settings");

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("<redacted>"));
        for private in [
            "OPENAI_API_KEY",
            "do-not-log",
            "experimental_bearer_token",
            "also-private",
        ] {
            assert!(!debug.contains(private));
        }
    }

    #[test]
    fn config_only_switch_projects_api_key_and_preserves_login_cache() {
        let auth = json!({"OPENAI_API_KEY": "secret"});
        let config = "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://example.com\"\n";
        let projected = prepare_provider_live_config(&auth, config).expect("valid config");
        let document = projected.parse::<DocumentMut>().expect("TOML");

        assert_eq!(
            document["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
            Some("secret")
        );
        assert!(!should_write_auth(Some("custom"), &auth, true));
        assert!(should_write_auth(Some("custom"), &auth, false));
    }

    #[test]
    fn official_auth_writes_only_material_and_stale_api_keys_are_distinct() {
        let oauth = json!({"tokens": {"access_token": "oauth"}});
        let stale = json!({"auth_mode": "apikey", "OPENAI_API_KEY": "old"});

        assert!(should_write_auth(Some("official"), &oauth, true));
        assert!(!should_write_auth(Some("official"), &json!({}), true));
        assert!(!live_auth_is_stale_third_party_residue(&oauth));
        assert!(live_auth_is_stale_third_party_residue(&stale));

        for malformed in [json!(42), json!({"unexpected": true}), json!(["value"])] {
            assert!(!auth_has_login_material(
                &json!({"OPENAI_API_KEY": malformed})
            ));
        }
        for malformed in [
            json!({"tokens": {"access_token": 42}}),
            json!({"personal_access_token": 42}),
            json!({"agent_identity": {"agent_runtime_id": "id"}}),
            json!({"bedrock_api_key": {"api_key": "secret"}}),
        ] {
            assert!(!auth_has_login_material(&malformed));
            assert!(!auth_has_credential_login_material(&malformed));
        }
        assert!(auth_has_login_material(&json!({"future_auth": true})));
        assert!(auth_has_credential_login_material(
            &json!({"future_auth": true})
        ));
        assert!(!auth_has_login_material(
            &json!({"last_refresh": "2026-08-28T00:00:00Z"})
        ));
        assert!(!live_auth_is_stale_third_party_residue(&json!({
            "OPENAI_API_KEY": "old",
            "future_auth": {"session": "valid"}
        })));
        assert!(auth_has_login_material(&json!({
            "agent_identity": {
                "agent_runtime_id": "runtime",
                "agent_private_key": "private"
            }
        })));
        assert!(auth_has_login_material(&json!({
            "bedrock_api_key": {"api_key": "secret", "region": "us-east-1"}
        })));
    }

    #[test]
    fn native_catalog_projects_models_pointer_and_gateway_sentinel() {
        let settings = json!({
            "modelCatalog": {"models": [
                {"model": "qwen3-coder-plus", "displayName": "Qwen", "contextWindow": 64000},
                {"model": "qwen3-coder-plus"}
            ]}
        });
        let projection = prepare_native_model_catalog(
            &settings,
            "model = \"qwen3-coder-plus\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://example.com\"\n",
            NativeCatalogOwnership::default(),
        )
        .expect("valid catalog");

        assert!(projection.managed);
        let document = projection.config.parse::<DocumentMut>().expect("TOML");
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some(MODEL_CATALOG_FILENAME)
        );
        assert_eq!(document["web_search"].as_str(), Some("disabled"));
        let models = projection.catalog.unwrap()["models"]
            .as_array()
            .expect("catalog models")
            .clone();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "qwen3-coder-plus");
        assert_eq!(models[0]["display_name"], "Qwen");
        assert_eq!(models[0]["context_window"], 64000);
        assert_eq!(models[0]["input_modalities"], json!(["text"]));
    }

    #[test]
    fn absent_catalog_does_not_claim_or_remove_existing_projection() {
        let config = "model_catalog_json = \"user-catalog.json\"\n";
        let projection =
            prepare_native_model_catalog(&json!({}), config, NativeCatalogOwnership::default())
                .expect("valid config");

        assert!(!projection.managed);
        assert_eq!(projection.config, config);
        assert!(projection.catalog.is_none());
    }

    #[test]
    fn catalog_pointer_ownership_requires_the_exact_written_value() {
        let custom = "/user/custom/cc-switch-model-catalog.json";
        for models in [json!([]), json!([{"model": "custom"}])] {
            let projection = prepare_native_model_catalog(
                &json!({"modelCatalog": {"models": models}}),
                &format!("model_catalog_json = {custom:?}\n"),
                NativeCatalogOwnership::default(),
            )
            .expect("valid custom pointer");
            assert_eq!(
                projection.config.parse::<DocumentMut>().unwrap()["model_catalog_json"].as_str(),
                Some(custom)
            );
        }
    }

    #[test]
    fn native_catalog_debug_redacts_projected_config() {
        let projection = PreparedNativeCatalog {
            config: "experimental_bearer_token = \"secret\"".to_owned(),
            catalog: Some(json!({"models": []})),
            managed: true,
        };
        let debug = format!("{projection:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("experimental_bearer_token"));
    }

    #[test]
    fn legacy_top_level_bearer_token_is_projected_into_the_active_provider() {
        let config = "model_provider = \"custom\"\nexperimental_bearer_token = \"legacy\"\n[model_providers.custom]\nbase_url = \"https://example.com\"\n";
        let projected = prepare_provider_live_config(&json!({}), config).expect("valid config");
        let document = projected.parse::<DocumentMut>().expect("TOML");
        assert_eq!(
            document["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
            Some("legacy")
        );
    }

    #[test]
    fn invalid_nonempty_catalog_rows_are_rejected_instead_of_clearing_state() {
        assert_eq!(
            prepare_native_model_catalog(
                &json!({"modelCatalog": {"models": [{"model": 42}]}}),
                "model_catalog_json = \"cc-switch-model-catalog.json\"\n",
                NativeCatalogOwnership::default(),
            ),
            Err(PrepareNativeLiveError::InvalidCatalogModel)
        );
    }

    #[test]
    fn manual_web_search_setting_is_preserved_without_ownership_evidence() {
        let projection = prepare_native_model_catalog(
            &json!({"modelCatalog": {"models": [{"model": "gpt-compatible"}]}}),
            "model = \"gpt-compatible\"\nweb_search = \"disabled\"\n",
            NativeCatalogOwnership::default(),
        )
        .expect("valid catalog");
        assert_eq!(
            projection.config.parse::<DocumentMut>().unwrap()["web_search"].as_str(),
            Some("disabled")
        );

        let cleared = prepare_native_model_catalog(
            &json!({"modelCatalog": {"models": []}}),
            "model_catalog_json = \"cc-switch-model-catalog.json\"\nweb_search = \"disabled\"\n",
            NativeCatalogOwnership {
                web_search_disabled: true,
            },
        )
        .expect("owned cleanup");
        assert!(cleared
            .config
            .parse::<DocumentMut>()
            .unwrap()
            .get("web_search")
            .is_none());
    }

    #[test]
    fn gateway_sentinel_does_not_overwrite_user_web_search_without_ownership() {
        let settings = json!({"modelCatalog": {"models": [{"model": "qwen3-coder-plus"}]}});
        assert_eq!(
            prepare_native_model_catalog(
                &settings,
                "model = \"qwen3-coder-plus\"\nweb_search = \"enabled\"\n",
                NativeCatalogOwnership::default(),
            ),
            Err(PrepareNativeLiveError::WebSearchConflict)
        );
        assert_eq!(
            prepare_native_model_catalog(
                &settings,
                "model = \"qwen3-coder-plus\"\nweb_search = \"enabled\"\n",
                NativeCatalogOwnership {
                    web_search_disabled: true,
                },
            ),
            Err(PrepareNativeLiveError::WebSearchConflict)
        );
        let owned = prepare_native_model_catalog(
            &settings,
            "model = \"qwen3-coder-plus\"\nweb_search = \"disabled\"\n",
            NativeCatalogOwnership {
                web_search_disabled: true,
            },
        )
        .expect("owned sentinel remains disabled");
        assert_eq!(
            owned.config.parse::<DocumentMut>().unwrap()["web_search"].as_str(),
            Some("disabled")
        );
    }

    #[test]
    fn deepseek_uses_the_official_catalog_and_trimmed_context_overrides() {
        let projection = prepare_native_model_catalog(
            &json!({"modelCatalog": {"models": [{
                "model": "deepseek-v4-pro",
                "contextWindow": " 64000 "
            }]}}),
            "model = \"deepseek-v4-pro\"\nmodel_provider = \"deepseek\"\n[model_providers.deepseek]\nbase_url = \"https://api.deepseek.com\"\n",
            NativeCatalogOwnership::default(),
        )
        .expect("official DeepSeek catalog");
        let models = projection.catalog.unwrap()["models"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(models[0]["context_window"], 64000);
        assert_eq!(models[0]["apply_patch_tool_type"], "freeform");
        assert!(models[0]["base_instructions"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
    }

    #[test]
    fn vendor_capabilities_require_a_real_domain_boundary() {
        for endpoint in [
            "https://deepseek.com.evil.example/v1",
            "https://evil.example/v1?upstream=deepseek.com",
            "https://deepseek.com@evil.example/v1",
            "https://evil.example\\@api.deepseek.com/v1",
        ] {
            let config = format!(
                "model = \"deepseek-v4-pro\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nbase_url = {endpoint:?}\n"
            );
            let projection = prepare_native_model_catalog(
                &json!({"modelCatalog": {"models": [{"model": "deepseek-v4-pro"}]}}),
                &config,
                NativeCatalogOwnership::default(),
            )
            .expect("neutral catalog");
            assert_ne!(
                projection.catalog.unwrap()["models"][0]["apply_patch_tool_type"],
                "freeform"
            );
        }

        assert!(endpoint_matches_any_domain(
            "https://api.deepseek.com/v1",
            DEEPSEEK_OFFICIAL_CATALOG_HOSTS
        ));
        assert!(!endpoint_matches_any_domain(
            "https://xiaomimimo.com.evil.example/v1",
            WEB_SEARCH_REJECT_HOSTS
        ));
    }
}
