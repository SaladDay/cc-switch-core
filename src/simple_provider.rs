//! Product-neutral simple provider forms and native projections.
//!
//! Products own presentation and persistence. This module owns only the
//! small field set that is useful to desktop, CLI, and other thin clients.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value as TomlValue};

use crate::{
    integration::{builtin_app_integration, builtin_app_integrations},
    AppType,
};

/// Semantic fields exposed by a simple provider editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SimpleProviderField {
    BaseUrl,
    ApiKey,
    Model,
}

impl fmt::Display for SimpleProviderField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BaseUrl => "baseUrl",
            Self::ApiKey => "apiKey",
            Self::Model => "model",
        })
    }
}

/// One semantic field declared by an application's simple form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleProviderFieldDescriptor {
    pub key: SimpleProviderField,
    pub required: bool,
}

impl SimpleProviderFieldDescriptor {
    const fn new(key: SimpleProviderField, required: bool) -> Self {
        Self { key, required }
    }
}

/// Default native protocol used when a new simple provider is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SimpleProviderProtocol {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "google-generative-ai")]
    GoogleGenerativeAi,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-chat-completions")]
    OpenAiChatCompletions,
}

/// Public, non-secret values supplied by a built-in preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub website_url: &'static str,
    pub brand_key: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
}

impl SimpleProviderPreset {
    const fn new(
        id: &'static str,
        name: &'static str,
        website_url: &'static str,
        brand_key: &'static str,
        base_url: &'static str,
        model: &'static str,
    ) -> Self {
        Self {
            id,
            name,
            website_url,
            brand_key,
            base_url,
            model,
        }
    }
}

/// Shared simple form for one built-in application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleProviderFormDescriptor {
    pub app_id: &'static str,
    pub default_protocol: SimpleProviderProtocol,
    /// Claude Code is deliberately locked to native Anthropic Messages.
    pub protocol_locked: bool,
    pub fields: &'static [SimpleProviderFieldDescriptor],
    pub presets: &'static [SimpleProviderPreset],
}

impl SimpleProviderFormDescriptor {
    const fn new(
        app_id: &'static str,
        default_protocol: SimpleProviderProtocol,
        protocol_locked: bool,
        fields: &'static [SimpleProviderFieldDescriptor],
        presets: &'static [SimpleProviderPreset],
    ) -> Self {
        Self {
            app_id,
            default_protocol,
            protocol_locked,
            fields,
            presets,
        }
    }
}

/// Editable values shared by thin provider clients.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimpleProviderValues {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

impl fmt::Debug for SimpleProviderValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SimpleProviderValues")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .finish()
    }
}

impl SimpleProviderValues {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

/// Rejection reason while reading or projecting a simple provider.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SimpleProviderError {
    #[error("provider name cannot be empty")]
    EmptyProviderName,
    #[error("simple provider field '{field}' is required for '{app_id}'")]
    MissingField {
        app_id: &'static str,
        field: SimpleProviderField,
    },
    #[error("native provider settings for '{app_id}' must be an object")]
    SettingsNotObject { app_id: &'static str },
    #[error("native provider field '{field}' is invalid for '{app_id}'")]
    InvalidNativeField {
        app_id: &'static str,
        field: &'static str,
    },
    #[error("native provider TOML is invalid for '{app_id}'")]
    InvalidToml { app_id: &'static str },
}

const BASE_OPTIONAL_KEY_REQUIRED_MODEL_OPTIONAL: &[SimpleProviderFieldDescriptor] = &[
    SimpleProviderFieldDescriptor::new(SimpleProviderField::BaseUrl, false),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::ApiKey, true),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::Model, false),
];
const BASE_KEY_REQUIRED: &[SimpleProviderFieldDescriptor] = &[
    SimpleProviderFieldDescriptor::new(SimpleProviderField::BaseUrl, true),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::ApiKey, true),
];
const BASE_OPTIONAL_KEY_MODEL_REQUIRED: &[SimpleProviderFieldDescriptor] = &[
    SimpleProviderFieldDescriptor::new(SimpleProviderField::BaseUrl, false),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::ApiKey, true),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::Model, true),
];
const BASE_KEY_MODEL_REQUIRED: &[SimpleProviderFieldDescriptor] = &[
    SimpleProviderFieldDescriptor::new(SimpleProviderField::BaseUrl, true),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::ApiKey, true),
    SimpleProviderFieldDescriptor::new(SimpleProviderField::Model, true),
];

const CLAUDE_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "kimi",
        "Kimi",
        "https://platform.kimi.com",
        "kimi",
        "https://api.moonshot.cn/anthropic",
        "kimi-k2.7-code",
    ),
    SimpleProviderPreset::new(
        "kimi-coding",
        "Kimi For Coding",
        "https://www.kimi.com/code/",
        "kimi",
        "https://api.kimi.com/coding/",
        "kimi-for-coding",
    ),
    SimpleProviderPreset::new(
        "deepseek",
        "DeepSeek",
        "https://platform.deepseek.com",
        "deepseek",
        "https://api.deepseek.com/anthropic",
        "deepseek-v4-pro",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api",
        "anthropic/claude-sonnet-5",
    ),
];

const CLAUDE_DESKTOP_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "kimi",
        "Kimi",
        "https://platform.kimi.com",
        "kimi",
        "https://api.moonshot.cn/anthropic",
        "",
    ),
    SimpleProviderPreset::new(
        "kimi-coding",
        "Kimi For Coding",
        "https://www.kimi.com/code/",
        "kimi",
        "https://api.kimi.com/coding/",
        "",
    ),
    SimpleProviderPreset::new(
        "deepseek",
        "DeepSeek",
        "https://platform.deepseek.com",
        "deepseek",
        "https://api.deepseek.com/anthropic",
        "",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api",
        "",
    ),
];

const CODEX_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "packycode",
        "PackyCode",
        "https://www.packyapi.ai",
        "packycode",
        "https://www.packyapi.ai/v1",
        "gpt-5.6-sol",
    ),
    SimpleProviderPreset::new(
        "deepseek",
        "DeepSeek",
        "https://platform.deepseek.com",
        "deepseek",
        "https://api.deepseek.com",
        "deepseek-v4-flash",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api/v1",
        "gpt-5.6-sol",
    ),
];

const GEMINI_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "packycode",
        "PackyCode",
        "https://www.packyapi.ai",
        "packycode",
        "https://www.packyapi.ai",
        "gemini-3.6-flash",
    ),
    SimpleProviderPreset::new(
        "apinebula",
        "APINebula",
        "https://apinebula.ai",
        "apinebula",
        "https://apinebula.ai",
        "gemini-3.6-flash",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api",
        "gemini-3.6-flash",
    ),
];

const GROKBUILD_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "packycode",
        "PackyCode",
        "https://www.packyapi.ai",
        "packycode",
        "https://www.packyapi.ai/v1",
        "grok-4.5",
    ),
    SimpleProviderPreset::new(
        "apinebula",
        "APINebula",
        "https://apinebula.ai",
        "apinebula",
        "https://apinebula.ai/v1",
        "grok-4.5",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api/v1",
        "x-ai/grok-4.5",
    ),
];

const OPENAI_COMPATIBLE_PRESETS: &[SimpleProviderPreset] = &[
    SimpleProviderPreset::new(
        "kimi",
        "Kimi",
        "https://platform.kimi.com",
        "kimi",
        "https://api.moonshot.cn/v1",
        "kimi-k2.7-code",
    ),
    SimpleProviderPreset::new(
        "deepseek",
        "DeepSeek",
        "https://platform.deepseek.com",
        "deepseek",
        "https://api.deepseek.com/v1",
        "deepseek-v4-flash",
    ),
    SimpleProviderPreset::new(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai",
        "openrouter",
        "https://openrouter.ai/api/v1",
        "anthropic/claude-sonnet-5",
    ),
];

pub(crate) static CLAUDE_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "claude",
    SimpleProviderProtocol::AnthropicMessages,
    true,
    BASE_OPTIONAL_KEY_REQUIRED_MODEL_OPTIONAL,
    CLAUDE_PRESETS,
);
pub(crate) static CLAUDE_DESKTOP_FORM: SimpleProviderFormDescriptor =
    SimpleProviderFormDescriptor::new(
        "claude-desktop",
        SimpleProviderProtocol::AnthropicMessages,
        false,
        BASE_KEY_REQUIRED,
        CLAUDE_DESKTOP_PRESETS,
    );
pub(crate) static CODEX_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "codex",
    SimpleProviderProtocol::OpenAiResponses,
    false,
    BASE_OPTIONAL_KEY_MODEL_REQUIRED,
    CODEX_PRESETS,
);
pub(crate) static GEMINI_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "gemini",
    SimpleProviderProtocol::GoogleGenerativeAi,
    false,
    BASE_OPTIONAL_KEY_REQUIRED_MODEL_OPTIONAL,
    GEMINI_PRESETS,
);
pub(crate) static GROKBUILD_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "grokbuild",
    SimpleProviderProtocol::OpenAiResponses,
    false,
    BASE_KEY_MODEL_REQUIRED,
    GROKBUILD_PRESETS,
);
pub(crate) static OPENCODE_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "opencode",
    SimpleProviderProtocol::OpenAiChatCompletions,
    false,
    BASE_KEY_MODEL_REQUIRED,
    OPENAI_COMPATIBLE_PRESETS,
);
pub(crate) static OPENCLAW_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "openclaw",
    SimpleProviderProtocol::OpenAiCompletions,
    false,
    BASE_KEY_MODEL_REQUIRED,
    OPENAI_COMPATIBLE_PRESETS,
);
pub(crate) static HERMES_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "hermes",
    SimpleProviderProtocol::OpenAiChatCompletions,
    false,
    BASE_KEY_MODEL_REQUIRED,
    OPENAI_COMPATIBLE_PRESETS,
);
pub(crate) static PI_FORM: SimpleProviderFormDescriptor = SimpleProviderFormDescriptor::new(
    "pi",
    SimpleProviderProtocol::OpenAiCompletions,
    false,
    BASE_KEY_MODEL_REQUIRED,
    OPENAI_COMPATIBLE_PRESETS,
);

/// Iterates over every built-in simple form in registry display order.
pub fn builtin_simple_provider_forms(
) -> impl ExactSizeIterator<Item = &'static SimpleProviderFormDescriptor> + DoubleEndedIterator + Clone
{
    builtin_app_integrations().map(|integration| integration.simple_provider_form())
}

/// Returns the simple form for one built-in application.
pub fn simple_provider_form(app: &AppType) -> &'static SimpleProviderFormDescriptor {
    builtin_app_integration(app).simple_provider_form()
}

/// Extracts simple values from an application's native provider shape.
pub fn extract_simple_provider_values(
    app: &AppType,
    settings: &Value,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let root = settings
        .as_object()
        .ok_or_else(|| settings_not_object(app))?;
    match app {
        AppType::Claude | AppType::ClaudeDesktop => extract_claude_like(app, root),
        AppType::Codex => extract_codex(root),
        AppType::Gemini => extract_gemini(root),
        AppType::GrokBuild => extract_grokbuild(root),
        AppType::OpenCode => extract_opencode(root),
        AppType::OpenClaw | AppType::Pi => extract_openai_array_provider(app, root),
        AppType::Hermes => extract_hermes(root),
    }
}

/// Projects simple values over optional native settings, preserving unknowns.
pub fn project_simple_provider_settings(
    app: &AppType,
    provider_name: &str,
    values: &SimpleProviderValues,
    existing: Option<&Value>,
) -> Result<Value, SimpleProviderError> {
    if provider_name.trim().is_empty() {
        return Err(SimpleProviderError::EmptyProviderName);
    }
    let values = normalize_and_validate(app, values)?;
    let mut root = existing_root(app, existing)?;
    match app {
        AppType::Claude => project_claude(&mut root, &values)?,
        AppType::ClaudeDesktop => project_claude_desktop(&mut root, &values)?,
        AppType::Codex => project_codex(&mut root, provider_name.trim(), &values)?,
        AppType::Gemini => project_gemini(&mut root, &values)?,
        AppType::GrokBuild => project_grokbuild(&mut root, provider_name.trim(), &values)?,
        AppType::OpenCode => project_opencode(&mut root, provider_name.trim(), &values)?,
        AppType::OpenClaw => project_openclaw(&mut root, &values)?,
        AppType::Hermes => project_hermes(&mut root, &values)?,
        AppType::Pi => project_pi(&mut root, &values)?,
    }
    Ok(Value::Object(root))
}

fn normalize_and_validate(
    app: &AppType,
    values: &SimpleProviderValues,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let values = SimpleProviderValues::new(
        values.base_url.trim(),
        values.api_key.trim(),
        values.model.trim(),
    );
    for field in simple_provider_form(app)
        .fields
        .iter()
        .filter(|field| field.required)
    {
        let missing = match field.key {
            SimpleProviderField::BaseUrl => values.base_url.is_empty(),
            // Grok Build also accepts an existing `env_key`. The native
            // projector verifies that credential after locating the selected
            // model table.
            SimpleProviderField::ApiKey => values.api_key.is_empty() && *app != AppType::GrokBuild,
            SimpleProviderField::Model => values.model.is_empty(),
        };
        if missing {
            return Err(SimpleProviderError::MissingField {
                app_id: stable_app_id(app),
                field: field.key,
            });
        }
    }
    Ok(values)
}

fn existing_root(
    app: &AppType,
    existing: Option<&Value>,
) -> Result<Map<String, Value>, SimpleProviderError> {
    match existing {
        Some(Value::Object(root)) => Ok(root.clone()),
        Some(_) => Err(settings_not_object(app)),
        None => Ok(Map::new()),
    }
}

fn settings_not_object(app: &AppType) -> SimpleProviderError {
    SimpleProviderError::SettingsNotObject {
        app_id: stable_app_id(app),
    }
}

fn invalid_native(app: &AppType, field: &'static str) -> SimpleProviderError {
    SimpleProviderError::InvalidNativeField {
        app_id: stable_app_id(app),
        field,
    }
}

fn stable_app_id(app: &AppType) -> &'static str {
    simple_provider_form(app).app_id
}

fn object_field_mut<'a>(
    app: &AppType,
    root: &'a mut Map<String, Value>,
    key: &'static str,
) -> Result<&'a mut Map<String, Value>, SimpleProviderError> {
    root.entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid_native(app, key))
}

fn set_optional_string(root: &mut Map<String, Value>, key: &str, value: &str) {
    if value.is_empty() {
        root.remove(key);
    } else {
        root.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn set_anthropic_credential(env: &mut Map<String, Value>, api_key: &str) {
    let uses_api_key =
        env.contains_key("ANTHROPIC_API_KEY") && !env.contains_key("ANTHROPIC_AUTH_TOKEN");
    let (credential_key, obsolete_key) = if uses_api_key {
        ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")
    } else {
        ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY")
    };
    env.insert(credential_key.to_owned(), Value::String(api_key.to_owned()));
    env.remove(obsolete_key);
}

fn extract_claude_like(
    app: &AppType,
    root: &Map<String, Value>,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let env = match root.get("env") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid_native(app, "env"))?,
        None => return Ok(SimpleProviderValues::new("", "", "")),
    };
    let api_key = env
        .get("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| env.get("ANTHROPIC_API_KEY"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(SimpleProviderValues::new(
        env.get("ANTHROPIC_BASE_URL")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        api_key,
        env.get("ANTHROPIC_MODEL")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ))
}

fn project_claude(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let env = object_field_mut(&AppType::Claude, root, "env")?;
    set_anthropic_credential(env, &values.api_key);
    set_optional_string(env, "ANTHROPIC_BASE_URL", &values.base_url);
    set_optional_string(env, "ANTHROPIC_MODEL", &values.model);
    for key in [
        "api_format",
        "apiFormat",
        "openrouter_compat_mode",
        "openrouterCompatMode",
    ] {
        root.remove(key);
    }
    Ok(())
}

fn project_claude_desktop(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let env = object_field_mut(&AppType::ClaudeDesktop, root, "env")?;
    env.insert(
        "ANTHROPIC_BASE_URL".to_owned(),
        Value::String(values.base_url.clone()),
    );
    env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_owned(),
        Value::String(values.api_key.clone()),
    );
    env.remove("ANTHROPIC_API_KEY");
    Ok(())
}

fn extract_codex(root: &Map<String, Value>) -> Result<SimpleProviderValues, SimpleProviderError> {
    let app = AppType::Codex;
    let auth = match root.get("auth") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid_native(&app, "auth"))?,
        None => &Map::new(),
    };
    let config = match root.get("config") {
        Some(Value::String(config)) => config.as_str(),
        Some(Value::Null) | None => "",
        Some(_) => return Err(invalid_native(&app, "config")),
    };
    let document = parse_toml(&app, config)?;
    let model = document
        .get("model")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let route_id = document.get("model_provider").and_then(Item::as_str);
    let route = route_id
        .and_then(|route_id| document.get("model_providers")?.get(route_id))
        .and_then(Item::as_table_like);
    let base_url = route
        .and_then(|route| route.get("base_url"))
        .and_then(Item::as_str)
        .or_else(|| document.get("base_url").and_then(Item::as_str))
        .unwrap_or_default();
    let api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .or_else(|| {
            route
                .and_then(|route| route.get("experimental_bearer_token"))
                .and_then(Item::as_str)
        })
        .or_else(|| {
            document
                .get("experimental_bearer_token")
                .and_then(Item::as_str)
        })
        .unwrap_or_default();
    Ok(SimpleProviderValues::new(base_url, api_key, model))
}

fn project_codex(
    root: &mut Map<String, Value>,
    provider_name: &str,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let app = AppType::Codex;
    object_field_mut(&app, root, "auth")?.insert(
        "OPENAI_API_KEY".to_owned(),
        Value::String(values.api_key.clone()),
    );
    let config = match root.get("config") {
        Some(Value::String(config)) => config.as_str(),
        Some(Value::Null) | None => "",
        Some(_) => return Err(invalid_native(&app, "config")),
    };
    let mut document = parse_toml(&app, config)?;
    let route_id = document
        .get("model_provider")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|route| !route.is_empty())
        .unwrap_or("custom")
        .to_owned();
    document["model_provider"] = toml_edit::value(&route_id);
    document["model"] = toml_edit::value(&values.model);
    let routes = toml_table_item(&app, &mut document, "model_providers")?;
    let route = nested_toml_table(&app, routes, &route_id, "model_providers")?;
    route.insert("name", toml_edit::value(provider_name));
    route.insert(
        "base_url",
        toml_edit::value(if values.base_url.is_empty() {
            "https://api.openai.com/v1"
        } else {
            &values.base_url
        }),
    );
    if route
        .get("wire_api")
        .and_then(Item::as_str)
        .is_none_or(|value| value.trim().is_empty())
    {
        route.insert("wire_api", toml_edit::value("responses"));
    }
    route.insert("requires_openai_auth", toml_edit::value(true));
    root.insert("config".to_owned(), Value::String(document.to_string()));
    Ok(())
}

fn extract_gemini(root: &Map<String, Value>) -> Result<SimpleProviderValues, SimpleProviderError> {
    let app = AppType::Gemini;
    let env = match root.get("env") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid_native(&app, "env"))?,
        None => return Ok(SimpleProviderValues::new("", "", "")),
    };
    Ok(SimpleProviderValues::new(
        env.get("GOOGLE_GEMINI_BASE_URL")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        env.get("GEMINI_API_KEY")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        env.get("GEMINI_MODEL")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ))
}

fn project_gemini(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let env = object_field_mut(&AppType::Gemini, root, "env")?;
    env.insert(
        "GEMINI_API_KEY".to_owned(),
        Value::String(values.api_key.clone()),
    );
    set_optional_string(env, "GOOGLE_GEMINI_BASE_URL", &values.base_url);
    set_optional_string(env, "GEMINI_MODEL", &values.model);
    Ok(())
}

fn extract_grokbuild(
    root: &Map<String, Value>,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let app = AppType::GrokBuild;
    let config = root
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_native(&app, "config"))?;
    let document = parse_toml(&app, config)?;
    let selected_key = document
        .get("models")
        .and_then(|models| models.get("default"))
        .and_then(Item::as_str)
        .unwrap_or_default();
    let selected = document
        .get("model")
        .and_then(|models| models.get(selected_key));
    Ok(SimpleProviderValues::new(
        selected
            .and_then(|model| model.get("base_url"))
            .and_then(Item::as_str)
            .unwrap_or_default(),
        selected
            .and_then(|model| model.get("api_key"))
            .and_then(Item::as_str)
            .unwrap_or_default(),
        selected
            .and_then(|model| model.get("model"))
            .and_then(Item::as_str)
            .unwrap_or_default(),
    ))
}

fn project_grokbuild(
    root: &mut Map<String, Value>,
    provider_name: &str,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let app = AppType::GrokBuild;
    let config = match root.get("config") {
        Some(Value::String(config)) => config.as_str(),
        Some(Value::Null) | None => "",
        Some(_) => return Err(invalid_native(&app, "config")),
    };
    let mut document = parse_toml(&app, config)?;
    let selected_key = document
        .get("models")
        .and_then(|models| models.get("default"))
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or(&values.model)
        .to_owned();
    toml_table(&app, &mut document, "models")?.insert("default", toml_edit::value(&selected_key));
    let model_tables = toml_table_item(&app, &mut document, "model")?;
    let selected = nested_toml_table(&app, model_tables, &selected_key, "model")?;
    selected.insert("model", toml_edit::value(&values.model));
    selected.insert("base_url", toml_edit::value(&values.base_url));
    selected.insert("name", toml_edit::value(provider_name));
    if values.api_key.is_empty() {
        let has_env_key = selected
            .get("env_key")
            .and_then(Item::as_str)
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        if !has_env_key {
            return Err(SimpleProviderError::MissingField {
                app_id: stable_app_id(&app),
                field: SimpleProviderField::ApiKey,
            });
        }
        selected.remove("api_key");
    } else {
        selected.insert("api_key", toml_edit::value(&values.api_key));
    }
    if selected
        .get("api_backend")
        .and_then(Item::as_str)
        .is_none_or(|value| value.trim().is_empty())
    {
        selected.insert("api_backend", toml_edit::value("responses"));
    }
    if selected
        .get("context_window")
        .and_then(Item::as_integer)
        .is_none_or(|value| value <= 0)
    {
        selected.insert("context_window", toml_edit::value(500_000));
    }
    root.insert("config".to_owned(), Value::String(document.to_string()));
    Ok(())
}

fn extract_opencode(
    root: &Map<String, Value>,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let app = AppType::OpenCode;
    let options = match root.get("options") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid_native(&app, "options"))?,
        None => &Map::new(),
    };
    let model = match root.get("models") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid_native(&app, "models"))?
            .keys()
            .next()
            .map(String::as_str)
            .unwrap_or_default(),
        None => "",
    };
    Ok(SimpleProviderValues::new(
        options
            .get("baseURL")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        options
            .get("apiKey")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        model,
    ))
}

fn project_opencode(
    root: &mut Map<String, Value>,
    provider_name: &str,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let app = AppType::OpenCode;
    root.entry("npm".to_owned())
        .or_insert_with(|| Value::String("@ai-sdk/openai-compatible".to_owned()));
    root.insert("name".to_owned(), Value::String(provider_name.to_owned()));
    let options = object_field_mut(&app, root, "options")?;
    options.insert("baseURL".to_owned(), Value::String(values.base_url.clone()));
    options.insert("apiKey".to_owned(), Value::String(values.api_key.clone()));
    let models = object_field_mut(&app, root, "models")?;
    select_object_model(models, &values.model);
    Ok(())
}

fn extract_openai_array_provider(
    app: &AppType,
    root: &Map<String, Value>,
) -> Result<SimpleProviderValues, SimpleProviderError> {
    let model = match root.get("models") {
        Some(Value::Array(models)) => models
            .first()
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        Some(_) => return Err(invalid_native(app, "models")),
        None => "",
    };
    Ok(SimpleProviderValues::new(
        root.get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        root.get("apiKey")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        model,
    ))
}

fn project_openclaw(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    project_openai_array_provider(&AppType::OpenClaw, root, values, true)
}

fn project_pi(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    project_openai_array_provider(&AppType::Pi, root, values, false)
}

fn project_openai_array_provider(
    app: &AppType,
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
    model_name: bool,
) -> Result<(), SimpleProviderError> {
    root.insert("baseUrl".to_owned(), Value::String(values.base_url.clone()));
    root.insert("apiKey".to_owned(), Value::String(values.api_key.clone()));
    root.entry("api".to_owned())
        .or_insert_with(|| Value::String("openai-completions".to_owned()));
    let models = root
        .entry("models".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| invalid_native(app, "models"))?;
    if models.is_empty() {
        models.push(Value::Object(Map::new()));
    }
    let first = models[0]
        .as_object_mut()
        .ok_or_else(|| invalid_native(app, "models"))?;
    first.insert("id".to_owned(), Value::String(values.model.clone()));
    if model_name {
        first
            .entry("name".to_owned())
            .or_insert_with(|| Value::String(values.model.clone()));
    }
    Ok(())
}

fn extract_hermes(root: &Map<String, Value>) -> Result<SimpleProviderValues, SimpleProviderError> {
    let app = AppType::Hermes;
    let model = match root.get("models") {
        Some(Value::Object(models)) => models.keys().next().map_or("", String::as_str),
        Some(Value::Array(models)) => models
            .first()
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        Some(_) => return Err(invalid_native(&app, "models")),
        None => "",
    };
    Ok(SimpleProviderValues::new(
        root.get("base_url")
            .or_else(|| root.get("baseUrl"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        root.get("api_key")
            .or_else(|| root.get("apiKey"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        model,
    ))
}

fn project_hermes(
    root: &mut Map<String, Value>,
    values: &SimpleProviderValues,
) -> Result<(), SimpleProviderError> {
    let app = AppType::Hermes;
    root.remove("baseUrl");
    root.remove("apiKey");
    root.insert(
        "base_url".to_owned(),
        Value::String(values.base_url.clone()),
    );
    root.insert("api_key".to_owned(), Value::String(values.api_key.clone()));
    if !root.contains_key("api_mode") {
        let protocol = root
            .remove("apiMode")
            .unwrap_or_else(|| Value::String("chat_completions".to_owned()));
        root.insert("api_mode".to_owned(), protocol);
    }
    let models = root
        .entry("models".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    match models {
        Value::Object(models) => {
            select_object_model(models, &values.model);
        }
        Value::Array(models) => {
            if models.is_empty() {
                models.push(Value::Object(Map::new()));
            }
            let first = models[0]
                .as_object_mut()
                .ok_or_else(|| invalid_native(&app, "models"))?;
            first.insert("id".to_owned(), Value::String(values.model.clone()));
            first
                .entry("name".to_owned())
                .or_insert_with(|| Value::String(values.model.clone()));
        }
        _ => return Err(invalid_native(&app, "models")),
    }
    Ok(())
}

fn select_object_model(models: &mut Map<String, Value>, model: &str) {
    let selected = models.remove(model).or_else(|| {
        models
            .keys()
            .next()
            .cloned()
            .and_then(|key| models.remove(&key))
    });
    let remaining = std::mem::take(models);
    models.insert(
        model.to_owned(),
        selected.unwrap_or_else(|| serde_json::json!({"name": model})),
    );
    models.extend(remaining);
}

fn parse_toml(app: &AppType, source: &str) -> Result<DocumentMut, SimpleProviderError> {
    source
        .parse::<DocumentMut>()
        .map_err(|_| SimpleProviderError::InvalidToml {
            app_id: stable_app_id(app),
        })
}

fn toml_table<'a>(
    app: &AppType,
    document: &'a mut DocumentMut,
    key: &'static str,
) -> Result<&'a mut dyn TableLike, SimpleProviderError> {
    toml_table_item(app, document, key)?
        .as_table_like_mut()
        .ok_or_else(|| invalid_native(app, key))
}

fn toml_table_item<'a>(
    app: &AppType,
    document: &'a mut DocumentMut,
    key: &'static str,
) -> Result<&'a mut Item, SimpleProviderError> {
    if document.get(key).is_none() {
        document[key] = Item::Table(Table::new());
    }
    let item = document
        .get_mut(key)
        .ok_or_else(|| invalid_native(app, key))?;
    if item.as_table_like().is_none() {
        return Err(invalid_native(app, key));
    }
    Ok(item)
}

fn nested_toml_table<'a>(
    app: &AppType,
    parent: &'a mut Item,
    key: &str,
    field: &'static str,
) -> Result<&'a mut dyn TableLike, SimpleProviderError> {
    if parent.get(key).is_none() {
        if let Some(parent) = parent.as_table_mut() {
            parent.insert(key, Item::Table(Table::new()));
        } else if let Some(parent) = parent.as_inline_table_mut() {
            parent.insert(key, TomlValue::InlineTable(InlineTable::new()));
        } else {
            return Err(invalid_native(app, field));
        }
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| invalid_native(app, field))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;
    use crate::{
        builtin_app_adapter, builtin_app_registry, claude, claude_desktop, codex, gemini,
        grokbuild, hermes, openclaw, opencode, pi,
    };

    fn sample_values(app: &AppType) -> SimpleProviderValues {
        let model = if *app == AppType::ClaudeDesktop {
            ""
        } else {
            "example-model"
        };
        SimpleProviderValues::new("https://example.com/v1", "secret", model)
    }

    #[test]
    fn forms_cover_the_registry_once_in_stable_order() {
        let registry_ids: Vec<_> = builtin_app_registry()
            .descriptors()
            .map(|descriptor| descriptor.id())
            .collect();
        let form_ids: Vec<_> = builtin_simple_provider_forms()
            .map(|form| form.app_id)
            .collect();
        assert_eq!(form_ids, registry_ids);

        for app in AppType::all() {
            let form = simple_provider_form(&app);
            assert_eq!(form.app_id, app.as_str());
            assert_eq!(builtin_app_adapter(&app).simple_provider_form(), form);
            assert!(form
                .fields
                .iter()
                .any(|field| { field.key == SimpleProviderField::ApiKey && field.required }));
            let mut preset_ids = HashSet::new();
            assert!(!form.presets.is_empty());
            for preset in form.presets {
                assert!(preset_ids.insert(preset.id));
                assert!(!preset.name.is_empty());
                assert!(!preset.base_url.is_empty());
            }
        }
        assert!(CLAUDE_FORM.protocol_locked);
        assert!(builtin_simple_provider_forms()
            .skip(1)
            .all(|form| !form.protocol_locked));
    }

    #[test]
    fn protocols_use_stable_wire_names() {
        let names = [
            (
                SimpleProviderProtocol::AnthropicMessages,
                "anthropic-messages",
            ),
            (SimpleProviderProtocol::OpenAiResponses, "openai-responses"),
            (
                SimpleProviderProtocol::GoogleGenerativeAi,
                "google-generative-ai",
            ),
            (
                SimpleProviderProtocol::OpenAiCompletions,
                "openai-completions",
            ),
            (
                SimpleProviderProtocol::OpenAiChatCompletions,
                "openai-chat-completions",
            ),
        ];

        for (protocol, expected) in names {
            assert_eq!(serde_json::to_value(protocol).unwrap(), expected);
        }
    }

    #[test]
    fn every_projection_round_trips_and_is_accepted_by_its_native_adapter() {
        for app in AppType::all() {
            let values = sample_values(&app);
            let settings = project_simple_provider_settings(&app, "Example", &values, None)
                .unwrap_or_else(|error| panic!("{}: {error}", app.as_str()));
            assert_eq!(
                extract_simple_provider_values(&app, &settings)
                    .unwrap_or_else(|error| panic!("{}: {error}", app.as_str())),
                values,
                "{}",
                app.as_str()
            );

            match app {
                AppType::Claude => {
                    claude::prepare_live_snapshot(&settings).expect("Claude settings");
                }
                AppType::ClaudeDesktop => {
                    claude_desktop::prepare_live_action(
                        &settings,
                        claude_desktop::ProviderMode::Direct,
                        None,
                    )
                    .expect("Claude Desktop settings");
                }
                AppType::Codex => {
                    let snapshot =
                        codex::prepare_strict_live_snapshot(&settings).expect("Codex settings");
                    codex::prepare_provider_live_config(
                        &snapshot.auth,
                        snapshot.config.as_deref().unwrap_or_default(),
                    )
                    .expect("Codex native config");
                }
                AppType::Gemini => {
                    gemini::prepare_live_snapshot(&settings, None, gemini::AuthMode::ApiKey)
                        .expect("Gemini settings");
                }
                AppType::GrokBuild => {
                    grokbuild::prepare_live_snapshot(&settings, grokbuild::ProviderMode::Custom)
                        .expect("Grok Build settings");
                }
                AppType::OpenCode => {
                    opencode::prepare_provider_entry("example", &settings)
                        .expect("OpenCode settings");
                }
                AppType::OpenClaw => {
                    openclaw::prepare_provider_entry("example", &settings)
                        .expect("OpenClaw settings");
                }
                AppType::Hermes => {
                    hermes::prepare_provider_entry("example", &settings).expect("Hermes settings");
                }
                AppType::Pi => {
                    pi::prepare_provider_entry("example", &settings).expect("Pi settings");
                }
            }
        }
    }

    #[test]
    fn projections_preserve_unknown_native_values() {
        for app in AppType::all() {
            let existing = match app {
                AppType::Claude | AppType::ClaudeDesktop | AppType::Gemini => {
                    json!({"env": {}, "future": {"keep": true}})
                }
                AppType::Codex => json!({
                    "auth": {"futureAuth": true},
                    "config": "future = true\n",
                    "future": {"keep": true}
                }),
                AppType::GrokBuild => json!({
                    "config": "future = true\n",
                    "future": {"keep": true}
                }),
                AppType::OpenCode => json!({"future": {"keep": true}}),
                AppType::OpenClaw | AppType::Hermes | AppType::Pi => {
                    json!({"future": {"keep": true}})
                }
            };
            let projected = project_simple_provider_settings(
                &app,
                "Example",
                &sample_values(&app),
                Some(&existing),
            )
            .unwrap_or_else(|error| panic!("{}: {error}", app.as_str()));
            assert_eq!(projected["future"]["keep"], true, "{}", app.as_str());
            if app == AppType::Codex {
                assert_eq!(projected["auth"]["futureAuth"], true);
                assert!(projected["config"]
                    .as_str()
                    .expect("config")
                    .contains("future = true"));
            }
            if app == AppType::GrokBuild {
                assert!(projected["config"]
                    .as_str()
                    .expect("config")
                    .contains("future = true"));
            }
        }
    }

    #[test]
    fn editing_an_object_model_makes_it_the_extracted_selection() {
        for app in [AppType::OpenCode, AppType::Hermes] {
            let existing = if app == AppType::OpenCode {
                json!({
                    "name": "Old name",
                    "options": {},
                    "models": {
                        "old-model": {"future": "keep"},
                        "other-model": {"other": true}
                    }
                })
            } else {
                json!({
                    "models": {
                        "old-model": {"future": "keep"},
                        "other-model": {"other": true}
                    }
                })
            };
            let values =
                SimpleProviderValues::new("https://example.com/v1", "secret", "selected-model");
            let projected =
                project_simple_provider_settings(&app, "Example", &values, Some(&existing))
                    .expect("projection");

            assert_eq!(
                extract_simple_provider_values(&app, &projected).expect("extraction"),
                values
            );
            assert_eq!(projected["models"]["selected-model"]["future"], "keep");
            assert_eq!(projected["models"]["other-model"]["other"], true);
            if app == AppType::OpenCode {
                assert_eq!(projected["name"], "Example");
            }
        }
    }

    #[test]
    fn editing_array_models_preserves_their_display_name() {
        for app in [AppType::OpenClaw, AppType::Hermes] {
            let existing = if app == AppType::OpenClaw {
                json!({
                    "baseUrl": "https://old.example/v1",
                    "apiKey": "old-key",
                    "api": "openai-completions",
                    "models": [{"id": "same-model", "name": "Friendly", "future": true}]
                })
            } else {
                json!({
                    "base_url": "https://old.example/v1",
                    "api_key": "old-key",
                    "api_mode": "chat_completions",
                    "models": [{"id": "same-model", "name": "Friendly", "future": true}]
                })
            };
            let projected = project_simple_provider_settings(
                &app,
                "Example",
                &SimpleProviderValues::new("https://old.example/v1", "new-key", "same-model"),
                Some(&existing),
            )
            .expect("projection");

            assert_eq!(projected["models"][0]["name"], "Friendly");
            assert_eq!(projected["models"][0]["future"], true);
        }
    }

    #[test]
    fn codex_and_grokbuild_edit_inline_toml_tables_in_place() {
        let codex = json!({
            "auth": {"OPENAI_API_KEY": "old-key"},
            "config": r#"model_provider = "custom"
model = "old-model"
model_providers = { custom = { name = "Old", base_url = "https://old.example/v1", wire_api = "responses", requires_openai_auth = true, future = "keep" } }
"#
        });
        let values = SimpleProviderValues::new("https://new.example/v1", "new-key", "new-model");
        let projected =
            project_simple_provider_settings(&AppType::Codex, "Renamed", &values, Some(&codex))
                .expect("Codex projection");
        let document = projected["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let route = document["model_providers"]["custom"]
            .as_table_like()
            .unwrap();
        assert_eq!(route.get("name").and_then(Item::as_str), Some("Renamed"));
        assert_eq!(route.get("future").and_then(Item::as_str), Some("keep"));
        assert_eq!(
            extract_simple_provider_values(&AppType::Codex, &projected).unwrap(),
            values
        );

        let grok = json!({
            "config": r#"models = { default = "custom" }
model = { custom = { model = "old-model", base_url = "https://old.example/v1", name = "Old", api_key = "old-key", api_backend = "responses", context_window = 500000, future = "keep" } }
"#
        });
        let projected =
            project_simple_provider_settings(&AppType::GrokBuild, "Renamed", &values, Some(&grok))
                .expect("Grok Build projection");
        let document = projected["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let model = document["model"]["custom"].as_table_like().unwrap();
        assert_eq!(model.get("name").and_then(Item::as_str), Some("Renamed"));
        assert_eq!(model.get("future").and_then(Item::as_str), Some("keep"));
        assert_eq!(
            extract_simple_provider_values(&AppType::GrokBuild, &projected).unwrap(),
            values
        );
    }

    #[test]
    fn codex_and_grokbuild_create_children_inside_empty_inline_tables() {
        let values = SimpleProviderValues::new("https://new.example/v1", "new-key", "new-model");
        let codex = project_simple_provider_settings(
            &AppType::Codex,
            "Inline",
            &values,
            Some(&json!({
                "auth": {},
                "config": "model_provider = \"custom\"\nmodel_providers = {}\n"
            })),
        )
        .expect("Codex projection");
        let document = codex["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            document["model_providers"]["custom"]["base_url"].as_str(),
            Some("https://new.example/v1")
        );

        let grok = project_simple_provider_settings(
            &AppType::GrokBuild,
            "Inline",
            &values,
            Some(&json!({
                "config": "models = { default = \"custom\" }\nmodel = {}\n"
            })),
        )
        .expect("Grok Build projection");
        let document = grok["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            document["model"]["custom"]["base_url"].as_str(),
            Some("https://new.example/v1")
        );
    }

    #[test]
    fn codex_extracts_top_level_fallbacks_and_active_route_tokens() {
        let top_level = json!({
            "auth": {},
            "config": r#"model = "old-model"
base_url = "https://top.example/v1"
experimental_bearer_token = "top-secret"
future = "keep"
"#
        });
        let values = extract_simple_provider_values(&AppType::Codex, &top_level).unwrap();
        assert_eq!(
            values,
            SimpleProviderValues::new("https://top.example/v1", "top-secret", "old-model")
        );
        let projected = project_simple_provider_settings(
            &AppType::Codex,
            "Top level",
            &values,
            Some(&top_level),
        )
        .unwrap();
        let document = projected["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            document["model_providers"]["custom"]["base_url"].as_str(),
            Some("https://top.example/v1")
        );
        assert_eq!(document["future"].as_str(), Some("keep"));

        let active_route = json!({
            "auth": {},
            "config": r#"model_provider = "custom"
model = "route-model"
experimental_bearer_token = "top-secret"

[model_providers.custom]
base_url = "https://route.example/v1"
experimental_bearer_token = "route-secret"
"#
        });
        assert_eq!(
            extract_simple_provider_values(&AppType::Codex, &active_route).unwrap(),
            SimpleProviderValues::new("https://route.example/v1", "route-secret", "route-model")
        );
    }

    #[test]
    fn grokbuild_preserves_an_existing_env_key_credential() {
        let existing = json!({
            "config": r#"[models]
default = "env-profile"

[model.env-profile]
model = "grok-4.5"
base_url = "https://old.example/v1"
name = "Environment"
env_key = "XAI_API_KEY"
api_backend = "responses"
context_window = 500000
"#
        });
        assert_eq!(
            extract_simple_provider_values(&AppType::GrokBuild, &existing).unwrap(),
            SimpleProviderValues::new("https://old.example/v1", "", "grok-4.5")
        );
        let projected = project_simple_provider_settings(
            &AppType::GrokBuild,
            "Environment",
            &SimpleProviderValues::new("https://new.example/v1", "", "grok-5"),
            Some(&existing),
        )
        .unwrap();
        let document = projected["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let selected = document["model"]["env-profile"].as_table_like().unwrap();
        assert_eq!(
            selected.get("env_key").and_then(Item::as_str),
            Some("XAI_API_KEY")
        );
        assert!(selected.get("api_key").is_none());
        assert_eq!(selected.get("model").and_then(Item::as_str), Some("grok-5"));
        grokbuild::prepare_live_snapshot(&projected, grokbuild::ProviderMode::Custom)
            .expect("Grok Build native config");
    }

    #[test]
    fn grokbuild_keeps_the_selected_table_alias_separate_from_the_model_id() {
        let existing = json!({
            "config": r#"[models]
default = "grok-custom"

[model.grok-custom]
model = "grok-4.5"
base_url = "https://old.example/v1"
name = "Old"
api_key = "old-key"
api_backend = "responses"
context_window = 500000
future = "keep"
"#
        });
        assert_eq!(
            extract_simple_provider_values(&AppType::GrokBuild, &existing).unwrap(),
            SimpleProviderValues::new("https://old.example/v1", "old-key", "grok-4.5")
        );

        let replacement = SimpleProviderValues::new("https://new.example/v1", "new-key", "grok-5");
        let projected = project_simple_provider_settings(
            &AppType::GrokBuild,
            "Renamed",
            &replacement,
            Some(&existing),
        )
        .unwrap();
        let document = projected["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();

        assert_eq!(document["models"]["default"].as_str(), Some("grok-custom"));
        assert_eq!(
            document["model"]["grok-custom"]["model"].as_str(),
            Some("grok-5")
        );
        assert_eq!(
            document["model"]["grok-custom"]["future"].as_str(),
            Some("keep")
        );
        assert_eq!(
            extract_simple_provider_values(&AppType::GrokBuild, &projected).unwrap(),
            replacement
        );
    }

    #[test]
    fn claude_simple_projection_is_always_native_anthropic_messages() {
        let projected = project_simple_provider_settings(
            &AppType::Claude,
            "Example",
            &SimpleProviderValues::new("https://example.com", "secret", "model"),
            Some(&json!({
                "apiFormat": "openai_responses",
                "openrouterCompatMode": true,
                "env": {"ANTHROPIC_API_KEY": "old", "future": "keep"}
            })),
        )
        .expect("Claude projection");

        assert!(projected.get("apiFormat").is_none());
        assert!(projected.get("openrouterCompatMode").is_none());
        assert_eq!(projected["env"]["ANTHROPIC_API_KEY"], "secret");
        assert!(projected["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert_eq!(projected["env"]["future"], "keep");
    }

    #[test]
    fn claude_projection_preserves_existing_credential_field() {
        let values = sample_values(&AppType::Claude);
        let projected = project_simple_provider_settings(
            &AppType::Claude,
            "Example",
            &values,
            Some(&json!({"env": {"ANTHROPIC_API_KEY": "old"}})),
        )
        .unwrap();
        assert_eq!(projected["env"]["ANTHROPIC_API_KEY"], values.api_key);
        assert!(projected["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());

        let created =
            project_simple_provider_settings(&AppType::Claude, "Example", &values, None).unwrap();
        assert_eq!(created["env"]["ANTHROPIC_AUTH_TOKEN"], values.api_key);
        assert!(created["env"].get("ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn claude_desktop_projection_keeps_its_required_auth_token() {
        let values = sample_values(&AppType::ClaudeDesktop);
        let projected = project_simple_provider_settings(
            &AppType::ClaudeDesktop,
            "Example",
            &values,
            Some(&json!({"env": {"ANTHROPIC_API_KEY": "old"}})),
        )
        .unwrap();

        assert_eq!(projected["env"]["ANTHROPIC_AUTH_TOKEN"], values.api_key);
        assert!(projected["env"].get("ANTHROPIC_API_KEY").is_none());
        claude_desktop::prepare_live_action(&projected, claude_desktop::ProviderMode::Direct, None)
            .expect("projected Claude Desktop settings");
    }

    #[test]
    fn validation_rejects_missing_declared_values_and_invalid_native_shapes() {
        assert_eq!(
            project_simple_provider_settings(
                &AppType::Pi,
                "Example",
                &SimpleProviderValues::new("", "secret", "model"),
                None,
            ),
            Err(SimpleProviderError::MissingField {
                app_id: "pi",
                field: SimpleProviderField::BaseUrl,
            })
        );
        assert_eq!(
            project_simple_provider_settings(
                &AppType::Claude,
                "Example",
                &SimpleProviderValues::new("", "", ""),
                None,
            ),
            Err(SimpleProviderError::MissingField {
                app_id: "claude",
                field: SimpleProviderField::ApiKey,
            })
        );
        assert!(matches!(
            extract_simple_provider_values(&AppType::Codex, &json!({"config": "not = [toml"})),
            Err(SimpleProviderError::InvalidToml { app_id: "codex" })
        ));
        assert!(matches!(
            project_simple_provider_settings(
                &AppType::OpenClaw,
                "Example",
                &sample_values(&AppType::OpenClaw),
                Some(&json!({"models": {}})),
            ),
            Err(SimpleProviderError::InvalidNativeField {
                app_id: "openclaw",
                field: "models"
            })
        ));
        assert_eq!(
            project_simple_provider_settings(
                &AppType::GrokBuild,
                "Example",
                &SimpleProviderValues::new("https://example.com/v1", "", "model"),
                None,
            ),
            Err(SimpleProviderError::MissingField {
                app_id: "grokbuild",
                field: SimpleProviderField::ApiKey,
            })
        );
    }

    #[test]
    fn debug_output_never_exposes_the_api_key() {
        let values = SimpleProviderValues::new("https://example.com", "do-not-log", "model");
        let debug = format!("{values:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
    }
}
