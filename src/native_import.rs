//! Pure projection from observed native documents into provider candidates.
//!
//! Hosts retain ownership of paths and file I/O. Projection may request one
//! logical target at a time so conditionally unrelated documents are never
//! observed merely to discover whether they are needed.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::{
    claude, claude_desktop, codex, gemini, grokbuild, hermes, openclaw, opencode, pi, AppType,
    LiveDocumentSet, LogicalTarget, NativeProviderMode, ProviderSnapshot,
};

const CLAUDE_DESKTOP_OFFICIAL_ID: &str = "claude-desktop-official";

/// Native Hermes section from which an imported provider originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HermesProviderSource {
    CustomProviders,
    ProvidersDictionary,
}

/// Typed app-specific context accompanying an imported provider.
#[derive(Clone, PartialEq, Eq)]
pub enum NativeImportContext {
    None,
    ClaudeDesktopDirect {
        routes: Vec<claude_desktop::DirectModelRoute>,
    },
    Hermes {
        source: HermesProviderSource,
    },
}

impl fmt::Debug for NativeImportContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::ClaudeDesktopDirect { routes } => formatter
                .debug_struct("ClaudeDesktopDirect")
                .field("route_count", &routes.len())
                .finish(),
            Self::Hermes { source } => formatter
                .debug_struct("Hermes")
                .field("source", source)
                .finish(),
        }
    }
}

/// One native provider candidate ready for consumer-owned persistence.
#[derive(Clone, PartialEq)]
pub struct NativeImportCandidate {
    pub provider: ProviderSnapshot,
    pub name_is_explicit: bool,
    pub classification: Option<NativeProviderMode>,
    pub context: NativeImportContext,
}

impl fmt::Debug for NativeImportCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeImportCandidate")
            .field("provider", &self.provider)
            .field("name_is_explicit", &self.name_is_explicit)
            .field("classification", &self.classification)
            .field("context", &self.context)
            .finish()
    }
}

/// One step in a pure native import projection.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeImportStep {
    /// The host must observe this logical target and retry with a new snapshot.
    Observe { target: LogicalTarget },
    /// Projection is complete and no more native documents are required.
    Ready {
        candidates: Vec<NativeImportCandidate>,
    },
}

/// Rejection reason while projecting native documents into providers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NativeImportError {
    #[error("live documents belong to '{actual}', expected '{expected}'")]
    WrongDocumentApp { expected: String, actual: String },
    #[error("live configuration is missing for {resource}")]
    Missing { resource: String },
    #[error("live document {target:?} is invalid: {message}")]
    InvalidDocument {
        target: LogicalTarget,
        message: String,
    },
    #[error("native import for '{app_id}' is invalid: {message}")]
    InvalidCandidate { app_id: String, message: String },
}

enum ProjectError {
    Observe(LogicalTarget),
    Rejected(NativeImportError),
}

impl From<NativeImportError> for ProjectError {
    fn from(error: NativeImportError) -> Self {
        Self::Rejected(error)
    }
}

type ProjectResult<T> = Result<T, ProjectError>;

pub(crate) fn project_native_import(
    adapter_app: &AppType,
    documents: &LiveDocumentSet,
) -> Result<NativeImportStep, NativeImportError> {
    if documents.app() != adapter_app {
        return Err(NativeImportError::WrongDocumentApp {
            expected: adapter_app.as_str().to_owned(),
            actual: documents.app().as_str().to_owned(),
        });
    }

    match project(adapter_app, documents) {
        Ok(candidates) => Ok(NativeImportStep::Ready { candidates }),
        Err(ProjectError::Observe(target)) => Ok(NativeImportStep::Observe { target }),
        Err(ProjectError::Rejected(error)) => Err(error),
    }
}

fn project(
    app: &AppType,
    documents: &LiveDocumentSet,
) -> ProjectResult<Vec<NativeImportCandidate>> {
    match app {
        AppType::Claude => import_claude(documents),
        AppType::Codex => import_codex(documents),
        AppType::Gemini => import_gemini(documents),
        AppType::GrokBuild => import_grokbuild(documents),
        AppType::OpenCode => import_json_entries(
            documents,
            app.clone(),
            LogicalTarget::OpenCodeConfig,
            &["provider"],
            "OpenCode",
        ),
        AppType::OpenClaw => import_json_entries(
            documents,
            app.clone(),
            LogicalTarget::OpenClawConfig,
            &["models", "providers"],
            "OpenClaw",
        ),
        AppType::ClaudeDesktop => import_claude_desktop(documents),
        AppType::Hermes => import_hermes(documents),
        AppType::Pi => import_json_entries(
            documents,
            app.clone(),
            LogicalTarget::PiModels,
            &["providers"],
            "Pi",
        ),
    }
}

fn import_claude(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let target = LogicalTarget::ClaudeSettings;
    let settings = required_json_object(documents, target, "Claude Code")?;
    let snapshot = claude::prepare_live_snapshot(&settings)
        .map_err(|error| invalid_document(target, error.to_string()))?;
    Ok(vec![candidate(
        AppType::Claude,
        "default",
        "Imported Claude Code",
        snapshot.settings,
        None,
        NativeImportContext::None,
    )?])
}

fn import_codex(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let config = optional_text(documents, LogicalTarget::CodexConfig, "Codex")?;
    let auth = optional_json_object(documents, LogicalTarget::CodexAuth, "Codex")?;
    if config.is_none() && auth.is_none() {
        return Err(missing("Codex"));
    }
    let settings = json!({
        "auth": auth.unwrap_or_else(|| json!({})),
        "config": config.unwrap_or_default()
    });
    codex::prepare_strict_live_snapshot(&settings)
        .map_err(|error| invalid_document(LogicalTarget::CodexConfig, error.to_string()))?;
    let official = codex_auth_is_official(&settings["auth"]);
    Ok(vec![candidate(
        AppType::Codex,
        if official {
            "codex-official"
        } else {
            "default"
        },
        "Imported Codex",
        settings,
        Some(if official {
            NativeProviderMode::Official
        } else {
            NativeProviderMode::Custom
        }),
        NativeImportContext::None,
    )?])
}

fn import_gemini(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let env_text = optional_text(documents, LogicalTarget::GeminiEnv, "Gemini")?;
    let config = optional_json_object(documents, LogicalTarget::GeminiSettings, "Gemini")?;
    if env_text.is_none() && config.is_none() {
        return Err(missing("Gemini"));
    }
    let env = parse_env(env_text.unwrap_or_default(), LogicalTarget::GeminiEnv)?;
    let settings = json!({"env": env, "config": config.unwrap_or_else(|| json!({}))});
    let mode = if settings
        .pointer("/config/security/auth/selectedType")
        .and_then(Value::as_str)
        == Some("oauth-personal")
    {
        gemini::AuthMode::OAuthPersonal
    } else {
        gemini::AuthMode::ApiKey
    };
    gemini::prepare_live_snapshot(&settings, None, mode)
        .map_err(|error| invalid_document(LogicalTarget::GeminiSettings, error.to_string()))?;
    let official = mode == gemini::AuthMode::OAuthPersonal;
    Ok(vec![candidate(
        AppType::Gemini,
        if official {
            "gemini-official"
        } else {
            "default"
        },
        if official {
            "Google"
        } else {
            "Imported Gemini"
        },
        settings,
        Some(if official {
            NativeProviderMode::Official
        } else {
            NativeProviderMode::Custom
        }),
        NativeImportContext::None,
    )?])
}

fn import_grokbuild(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let target = LogicalTarget::GrokConfig;
    let config =
        optional_text(documents, target, "Grok Build")?.ok_or_else(|| missing("Grok Build"))?;
    let settings = json!({"config": config});
    let mode = if grok_config_is_official(settings["config"].as_str().unwrap_or_default()) {
        grokbuild::ProviderMode::Official
    } else {
        grokbuild::ProviderMode::Custom
    };
    grokbuild::prepare_live_snapshot(&settings, mode)
        .map_err(|error| invalid_document(target, error.to_string()))?;
    let official = mode == grokbuild::ProviderMode::Official;
    Ok(vec![candidate(
        AppType::GrokBuild,
        if official {
            "grokbuild-official"
        } else {
            "default"
        },
        "Imported Grok Build",
        settings,
        Some(if official {
            NativeProviderMode::Official
        } else {
            NativeProviderMode::Custom
        }),
        NativeImportContext::None,
    )?])
}

fn import_json_entries(
    documents: &LiveDocumentSet,
    app: AppType,
    target: LogicalTarget,
    keys: &[&str],
    label: &str,
) -> ProjectResult<Vec<NativeImportCandidate>> {
    let root = required_json5_object(documents, target, label)?;
    let entries = nested_object(&root, keys).ok_or_else(|| missing(label))?;
    let mut candidates = Vec::with_capacity(entries.len());
    for (key, settings) in entries {
        let valid = match app {
            AppType::OpenCode => opencode::prepare_provider_entry(key, settings).is_ok(),
            AppType::OpenClaw => openclaw::prepare_provider_entry(key, settings).is_ok(),
            AppType::Pi => pi::prepare_provider_entry(key, settings).is_ok(),
            _ => false,
        };
        if !valid {
            return Err(invalid_document(
                target,
                format!("{label} provider '{key}' is invalid"),
            ));
        }
        let name = settings
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(key);
        candidates.push(candidate(
            app.clone(),
            key,
            name,
            settings.clone(),
            None,
            NativeImportContext::None,
        )?);
    }
    if candidates.is_empty() {
        return Err(missing(format!("{label} providers")));
    }
    Ok(candidates)
}

fn import_claude_desktop(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let profile = optional_json_object(
        documents,
        LogicalTarget::ClaudeDesktopProfile,
        "Claude Desktop",
    )?;
    let Some(profile) = profile else {
        let normal = optional_json_object(
            documents,
            LogicalTarget::ClaudeDesktopNormalConfig,
            "Claude Desktop",
        )?;
        let normal_is_official = normal
            .as_ref()
            .is_some_and(|value| value.get("deploymentMode").and_then(Value::as_str) == Some("1p"));
        let official = if normal_is_official {
            true
        } else {
            optional_json_object(
                documents,
                LogicalTarget::ClaudeDesktopThreepConfig,
                "Claude Desktop",
            )?
            .as_ref()
            .is_some_and(|value| value.get("deploymentMode").and_then(Value::as_str) == Some("1p"))
        };
        if !official {
            return Err(missing("Claude Desktop direct profile"));
        }
        return Ok(vec![candidate(
            AppType::ClaudeDesktop,
            CLAUDE_DESKTOP_OFFICIAL_ID,
            "Claude Desktop Official",
            json!({}),
            Some(NativeProviderMode::Official),
            NativeImportContext::None,
        )?]);
    };

    let base_url = profile
        .get("inferenceGatewayBaseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            invalid_document(
                LogicalTarget::ClaudeDesktopProfile,
                "Claude Desktop profile has no gateway URL",
            )
        })?;
    let token = profile
        .get("inferenceGatewayApiKey")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            invalid_document(
                LogicalTarget::ClaudeDesktopProfile,
                "Claude Desktop profile has no gateway token",
            )
        })?;
    let settings = json!({
        "env": {
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_AUTH_TOKEN": token
        }
    });
    let routes = desktop_routes_from_profile(&profile)?;
    claude_desktop::prepare_live_action(
        &settings,
        claude_desktop::ProviderMode::Direct,
        Some(&routes),
    )
    .map_err(|error| invalid_document(LogicalTarget::ClaudeDesktopProfile, error.to_string()))?;
    Ok(vec![candidate(
        AppType::ClaudeDesktop,
        "default",
        "Imported Claude Desktop",
        settings,
        Some(NativeProviderMode::Custom),
        NativeImportContext::ClaudeDesktopDirect { routes },
    )?])
}

fn import_hermes(documents: &LiveDocumentSet) -> ProjectResult<Vec<NativeImportCandidate>> {
    let target = LogicalTarget::HermesConfig;
    let raw = optional_text(documents, target, "Hermes")?.ok_or_else(|| missing("Hermes"))?;
    let root = parse_yaml(raw, target)?;
    let mut providers = Map::new();
    let mut sources = HashMap::new();

    if let Some(section) = root.get("custom_providers") {
        let entries = section.as_sequence().ok_or_else(|| {
            invalid_document(target, "Hermes custom_providers must be a sequence")
        })?;
        for entry in entries {
            let name = entry
                .get("name")
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid_document(
                        target,
                        "Hermes custom_providers contains an unnamed provider",
                    )
                })?;
            let mut value = yaml_to_json(entry, target)?;
            if !value.is_object() {
                return Err(invalid_document(
                    target,
                    format!("Hermes provider '{name}' is invalid"),
                ));
            }
            denormalize_hermes_models(&mut value);
            if providers.insert(name.to_owned(), value).is_some() {
                return Err(invalid_document(
                    target,
                    format!("Hermes provider '{name}' is defined more than once"),
                ));
            }
            sources.insert(name.to_owned(), HermesProviderSource::CustomProviders);
        }
    }

    if let Some(section) = root.get("providers") {
        let entries = section
            .as_mapping()
            .ok_or_else(|| invalid_document(target, "Hermes providers must be a mapping"))?;
        let mut dictionary_names = HashSet::new();
        for (key, entry) in entries {
            let key = key
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid_document(target, "Hermes providers contains an invalid provider key")
                })?;
            let mut value = yaml_to_json(entry, target)?;
            let object = value.as_object_mut().ok_or_else(|| {
                invalid_document(target, format!("Hermes provider '{key}' is invalid"))
            })?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(key)
                .to_owned();
            if !dictionary_names.insert(name.clone()) {
                return Err(invalid_document(
                    target,
                    format!("Hermes provider '{name}' is defined more than once"),
                ));
            }
            if providers.contains_key(&name) {
                continue;
            }
            object.insert("name".to_owned(), json!(name));
            object.insert("provider_key".to_owned(), json!(key));
            denormalize_hermes_models(&mut value);
            sources.insert(name.clone(), HermesProviderSource::ProvidersDictionary);
            providers.insert(name, value);
        }
    }

    let mut candidates = Vec::with_capacity(providers.len());
    for (name, settings) in providers {
        if hermes::prepare_provider_entry(&name, &settings).is_err() {
            return Err(invalid_document(
                target,
                format!("Hermes provider '{name}' is invalid"),
            ));
        }
        let source = sources
            .remove(&name)
            .ok_or_else(|| NativeImportError::InvalidCandidate {
                app_id: AppType::Hermes.as_str().to_owned(),
                message: "provider source is missing".to_owned(),
            })?;
        candidates.push(candidate(
            AppType::Hermes,
            &name,
            &name,
            settings,
            None,
            NativeImportContext::Hermes { source },
        )?);
    }
    if candidates.is_empty() {
        return Err(missing("Hermes providers"));
    }
    Ok(candidates)
}

fn candidate(
    app: AppType,
    id: impl Into<String>,
    name: impl Into<String>,
    settings: Value,
    classification: Option<NativeProviderMode>,
    context: NativeImportContext,
) -> ProjectResult<NativeImportCandidate> {
    if !settings.is_object() {
        return Err(NativeImportError::InvalidCandidate {
            app_id: app.as_str().to_owned(),
            message: "provider settings must be an object".to_owned(),
        }
        .into());
    }
    let name_is_explicit = app.is_additive_mode()
        && settings
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|name| !name.is_empty());
    Ok(NativeImportCandidate {
        provider: ProviderSnapshot::new(id, app, name, settings),
        name_is_explicit,
        classification,
        context,
    })
}

fn observed_contents(
    documents: &LiveDocumentSet,
    target: LogicalTarget,
) -> ProjectResult<Option<&[u8]>> {
    let document =
        documents
            .document(target)
            .ok_or_else(|| NativeImportError::InvalidDocument {
                target,
                message: "target is absent from the document inventory".to_owned(),
            })?;
    if !document.is_observed() {
        return Err(ProjectError::Observe(target));
    }
    Ok(document.contents())
}

fn optional_text<'a>(
    documents: &'a LiveDocumentSet,
    target: LogicalTarget,
    label: &str,
) -> ProjectResult<Option<&'a str>> {
    observed_contents(documents, target)?
        .map(|contents| {
            std::str::from_utf8(contents)
                .map_err(|_| invalid_document(target, format!("{label} config is not UTF-8")))
        })
        .transpose()
}

fn optional_json_object(
    documents: &LiveDocumentSet,
    target: LogicalTarget,
    label: &str,
) -> ProjectResult<Option<Value>> {
    observed_contents(documents, target)?
        .map(|contents| parse_json_object(contents, target, label))
        .transpose()
}

fn required_json_object(
    documents: &LiveDocumentSet,
    target: LogicalTarget,
    label: &str,
) -> ProjectResult<Value> {
    optional_json_object(documents, target, label)?.ok_or_else(|| missing(label))
}

fn parse_json_object(contents: &[u8], target: LogicalTarget, label: &str) -> ProjectResult<Value> {
    let value: Value = serde_json::from_slice(contents)
        .map_err(|_| invalid_document(target, format!("{label} JSON could not be parsed")))?;
    if !value.is_object() {
        return Err(invalid_document(
            target,
            format!("{label} JSON root must be an object"),
        ));
    }
    Ok(value)
}

fn required_json5_object(
    documents: &LiveDocumentSet,
    target: LogicalTarget,
    label: &str,
) -> ProjectResult<Map<String, Value>> {
    let contents = observed_contents(documents, target)?.ok_or_else(|| missing(label))?;
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid_document(target, format!("{label} config is not UTF-8")))?;
    let value: Value = json5::from_str(text)
        .map_err(|_| invalid_document(target, format!("{label} JSON5 could not be parsed")))?;
    value.as_object().cloned().ok_or_else(|| {
        invalid_document(
            target,
            format!("{label} configuration root must be an object"),
        )
    })
}

fn parse_env(contents: &str, target: LogicalTarget) -> ProjectResult<Map<String, Value>> {
    let mut env = Map::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            invalid_document(
                target,
                format!("Gemini .env line {} has no '=' separator", index + 1),
            )
        })?;
        let key = key.trim();
        if !valid_env_key(key) {
            return Err(invalid_document(
                target,
                format!(
                    "Gemini .env line {} has an invalid variable name",
                    index + 1
                ),
            ));
        }
        let value = value.trim();
        if value.contains(['\r', '\n', '\0']) {
            return Err(invalid_document(
                target,
                format!("Gemini .env line {} has an invalid value", index + 1),
            ));
        }
        env.insert(key.to_owned(), Value::String(value.to_owned()));
    }
    Ok(env)
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn nested_object<'a>(
    root: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Map<String, Value>> {
    let mut current = root;
    for key in keys {
        current = current.get(*key)?.as_object()?;
    }
    Some(current)
}

fn grok_config_is_official(config: &str) -> bool {
    config
        .parse::<toml_edit::DocumentMut>()
        .is_ok_and(|document| !document.contains_key("models") && !document.contains_key("model"))
}

fn codex_auth_is_official(auth: &Value) -> bool {
    let has_api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    !has_api_key && codex::auth_has_login_material(auth)
}

fn desktop_routes_from_profile(
    profile: &Value,
) -> ProjectResult<Vec<claude_desktop::DirectModelRoute>> {
    let target = LogicalTarget::ClaudeDesktopProfile;
    let mut metadata = Map::new();
    if let Some(entries) = profile.get("inferenceModels") {
        let entries = entries.as_array().ok_or_else(|| {
            invalid_document(target, "Claude Desktop inferenceModels must be an array")
        })?;
        for entry in entries {
            let (name, label, supports_1m) = match entry {
                Value::String(name) => (name.as_str(), None, false),
                Value::Object(entry) => {
                    let name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
                        invalid_document(target, "Claude Desktop model route is missing its name")
                    })?;
                    (
                        name,
                        entry.get("labelOverride").and_then(Value::as_str),
                        entry
                            .get("supports1m")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                }
                _ => {
                    return Err(invalid_document(
                        target,
                        "Claude Desktop model route is invalid",
                    ));
                }
            };
            let name = name.trim();
            if name.is_empty() {
                return Err(invalid_document(
                    target,
                    "Claude Desktop model route is empty",
                ));
            }
            let mut route = json!({"model": name});
            if let Some(label) = label.map(str::trim).filter(|value| !value.is_empty()) {
                route["labelOverride"] = json!(label);
            }
            if supports_1m {
                route["supports1m"] = json!(true);
            }
            metadata.insert(name.to_owned(), route);
        }
    }
    metadata
        .iter()
        .map(|(route_id, value)| {
            let model = value.get("model").and_then(Value::as_str).ok_or_else(|| {
                invalid_document(
                    target,
                    "Claude Desktop model route is missing its upstream model",
                )
            })?;
            Ok(claude_desktop::DirectModelRoute {
                route_id: route_id.clone(),
                upstream_model: model.to_owned(),
                label_override: value
                    .get("labelOverride")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                supports_1m: value
                    .get("supports1m")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn parse_yaml(contents: &str, target: LogicalTarget) -> ProjectResult<serde_yaml::Value> {
    if contents.trim().is_empty() {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    if let Some(key) = duplicate_yaml_top_level_key(contents) {
        return Err(invalid_document(
            target,
            format!("Hermes YAML contains duplicate top-level key '{key}'"),
        ));
    }
    let value: serde_yaml::Value = serde_yaml::from_str(contents)
        .map_err(|_| invalid_document(target, "Hermes YAML could not be parsed"))?;
    if !value.is_mapping() {
        return Err(invalid_document(
            target,
            "Hermes YAML root must be a mapping",
        ));
    }
    Ok(value)
}

fn yaml_to_json(value: &serde_yaml::Value, target: LogicalTarget) -> ProjectResult<Value> {
    serde_json::to_value(value)
        .map_err(|_| invalid_document(target, "Hermes provider is not JSON-compatible"))
}

fn denormalize_hermes_models(value: &mut Value) {
    let Some(models) = value
        .as_object_mut()
        .and_then(|object| object.get_mut("models"))
    else {
        return;
    };
    let Some(entries) = models.as_object() else {
        return;
    };
    *models = Value::Array(
        entries
            .iter()
            .filter_map(|(id, value)| {
                let mut value = match value {
                    Value::Object(value) => value.clone(),
                    Value::Null => Map::new(),
                    _ => return None,
                };
                value.insert("id".to_owned(), json!(id));
                Some(Value::Object(value))
            })
            .collect(),
    );
}

fn duplicate_yaml_top_level_key(raw: &str) -> Option<String> {
    let mut seen = HashSet::new();
    for line in raw.split('\n') {
        if yaml_top_level_key_line(line) {
            if let Some(colon) = line.find(':') {
                let key = line[..colon].trim();
                if !seen.insert(key) {
                    return Some(key.to_owned());
                }
            }
        }
    }
    None
}

fn yaml_top_level_key_line(line: &str) -> bool {
    if line.is_empty() || line.starts_with([' ', '\t', '#', '-']) {
        return false;
    }
    line.find(':').is_some_and(|colon| {
        let suffix = &line[colon + 1..];
        suffix.is_empty() || suffix.starts_with([' ', '\t', '\r'])
    })
}

fn missing(resource: impl Into<String>) -> ProjectError {
    NativeImportError::Missing {
        resource: resource.into(),
    }
    .into()
}

fn invalid_document(target: LogicalTarget, message: impl Into<String>) -> ProjectError {
    NativeImportError::InvalidDocument {
        target,
        message: message.into(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin_app_adapter, ObservedDocument};

    fn documents(app: AppType, observed: &[(LogicalTarget, Option<&str>)]) -> LiveDocumentSet {
        let documents = builtin_app_adapter(&app)
            .targets()
            .iter()
            .copied()
            .map(|target| {
                observed
                    .iter()
                    .find(|(candidate, _)| *candidate == target)
                    .map_or_else(
                        || ObservedDocument::unobserved(target),
                        |(_, contents)| {
                            contents.map_or_else(
                                || ObservedDocument::missing(target),
                                |contents| ObservedDocument::present(target, contents.as_bytes()),
                            )
                        },
                    )
            });
        LiveDocumentSet::try_new(app, documents).expect("complete target inventory")
    }

    fn ready(
        app: AppType,
        observed: &[(LogicalTarget, Option<&str>)],
    ) -> Vec<NativeImportCandidate> {
        let documents = documents(app.clone(), observed);
        match project_native_import(&app, &documents).expect("valid import") {
            NativeImportStep::Ready { candidates } => candidates,
            NativeImportStep::Observe { target } => panic!("unexpected observation: {target:?}"),
        }
    }

    #[test]
    fn projection_requests_only_the_next_document_it_consumes() {
        let app = AppType::ClaudeDesktop;
        let adapter = builtin_app_adapter(&app);
        let initial = documents(app.clone(), &[]);
        assert_eq!(
            adapter.project_native_import(&initial).unwrap(),
            NativeImportStep::Observe {
                target: LogicalTarget::ClaudeDesktopProfile
            }
        );

        let direct = documents(
            app.clone(),
            &[(
                LogicalTarget::ClaudeDesktopProfile,
                Some(
                    r#"{"inferenceGatewayBaseUrl":"https://example.com","inferenceGatewayApiKey":"secret"}"#,
                ),
            )],
        );
        assert!(matches!(
            adapter.project_native_import(&direct).unwrap(),
            NativeImportStep::Ready { .. }
        ));

        let missing_profile =
            documents(app.clone(), &[(LogicalTarget::ClaudeDesktopProfile, None)]);
        assert_eq!(
            adapter.project_native_import(&missing_profile).unwrap(),
            NativeImportStep::Observe {
                target: LogicalTarget::ClaudeDesktopNormalConfig
            }
        );
        let normal = documents(
            app.clone(),
            &[
                (LogicalTarget::ClaudeDesktopProfile, None),
                (
                    LogicalTarget::ClaudeDesktopNormalConfig,
                    Some(r#"{"deploymentMode":"1p"}"#),
                ),
            ],
        );
        assert!(matches!(
            adapter.project_native_import(&normal).unwrap(),
            NativeImportStep::Ready { .. }
        ));

        let undecided_normal = documents(
            app,
            &[
                (LogicalTarget::ClaudeDesktopProfile, None),
                (
                    LogicalTarget::ClaudeDesktopNormalConfig,
                    Some(r#"{"deploymentMode":"3p"}"#),
                ),
            ],
        );
        assert_eq!(
            adapter.project_native_import(&undecided_normal).unwrap(),
            NativeImportStep::Observe {
                target: LogicalTarget::ClaudeDesktopThreepConfig
            }
        );
    }

    #[test]
    fn exclusive_apps_project_stable_native_identity_and_classification() {
        let claude = ready(
            AppType::Claude,
            &[(
                LogicalTarget::ClaudeSettings,
                Some(r#"{"env":{"TOKEN":"secret"}}"#),
            )],
        )
        .remove(0);
        assert_eq!(claude.provider.id, "default");
        assert_eq!(claude.provider.settings["env"]["TOKEN"], "secret");

        let codex = ready(
            AppType::Codex,
            &[
                (LogicalTarget::CodexConfig, Some("model = \"gpt-5\"\n")),
                (
                    LogicalTarget::CodexAuth,
                    Some(r#"{"tokens":{"access_token":"oauth"}}"#),
                ),
            ],
        )
        .remove(0);
        assert_eq!(codex.provider.id, "codex-official");
        assert_eq!(codex.classification, Some(NativeProviderMode::Official));

        let gemini = ready(
            AppType::Gemini,
            &[
                (LogicalTarget::GeminiEnv, Some("GEMINI_API_KEY=secret\n")),
                (
                    LogicalTarget::GeminiSettings,
                    Some(r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#),
                ),
            ],
        )
        .remove(0);
        assert_eq!(gemini.provider.id, "default");
        assert_eq!(gemini.classification, Some(NativeProviderMode::Custom));

        let grok = ready(
            AppType::GrokBuild,
            &[(
                LogicalTarget::GrokConfig,
                Some("[mcp_servers.keep]\ncommand = \"keep\"\n"),
            )],
        )
        .remove(0);
        assert_eq!(grok.provider.id, "grokbuild-official");
        assert_eq!(grok.classification, Some(NativeProviderMode::Official));
    }

    #[test]
    fn additive_json_apps_preserve_native_keys_and_settings() {
        let cases = [
            (
                AppType::OpenCode,
                LogicalTarget::OpenCodeConfig,
                r#"{provider:{custom:{name:'Custom',npm:'package'}}}"#,
            ),
            (
                AppType::OpenClaw,
                LogicalTarget::OpenClawConfig,
                r#"{models:{providers:{custom:{name:'Custom',models:[]}}}}"#,
            ),
            (
                AppType::Pi,
                LogicalTarget::PiModels,
                r#"{providers:{custom:{name:'Custom',models:[]}}}"#,
            ),
        ];
        for (app, target, source) in cases {
            let imported = ready(app.clone(), &[(target, Some(source))]).remove(0);
            assert_eq!(imported.provider.app, app);
            assert_eq!(imported.provider.id, "custom");
            assert_eq!(imported.provider.name, "Custom");
            assert!(imported.name_is_explicit);
        }
    }

    #[test]
    fn additive_import_preserves_all_candidates_from_a_bounded_document() {
        let providers = (0..=4096)
            .map(|index| (format!("p{index}"), json!({})))
            .collect::<Map<_, _>>();
        let source = json!({"providers": providers}).to_string();

        let imported = ready(
            AppType::Pi,
            &[(LogicalTarget::PiModels, Some(source.as_str()))],
        );

        assert_eq!(imported.len(), 4097);
    }

    #[test]
    fn desktop_and_hermes_return_typed_app_context() {
        let desktop = ready(
            AppType::ClaudeDesktop,
            &[(
                LogicalTarget::ClaudeDesktopProfile,
                Some(
                    r#"{"inferenceGatewayBaseUrl":"https://example.com","inferenceGatewayApiKey":"secret","inferenceModels":[{"name":"claude-sonnet-4-6","labelOverride":"Sonnet","supports1m":true}]}"#,
                ),
            )],
        )
        .remove(0);
        assert!(matches!(
            desktop.context,
            NativeImportContext::ClaudeDesktopDirect { ref routes }
                if routes.len() == 1 && routes[0].supports_1m
        ));

        let hermes = ready(
            AppType::Hermes,
            &[(
                LogicalTarget::HermesConfig,
                Some(
                    "custom_providers:\n  - name: custom\n    base_url: https://custom.example.com\nproviders:\n  dictionary:\n    base_url: https://dictionary.example.com\n",
                ),
            )],
        );
        assert_eq!(hermes.len(), 2);
        assert!(hermes.iter().any(|candidate| matches!(
            candidate.context,
            NativeImportContext::Hermes {
                source: HermesProviderSource::CustomProviders
            }
        )));
        assert!(hermes.iter().any(|candidate| matches!(
            candidate.context,
            NativeImportContext::Hermes {
                source: HermesProviderSource::ProvidersDictionary
            }
        )));
        assert!(hermes.iter().all(|candidate| candidate
            .provider
            .settings
            .get("_cc_source")
            .is_none()));
    }

    #[test]
    fn errors_and_debug_output_do_not_expose_native_secrets() {
        let app = AppType::Claude;
        let candidates = ready(
            app.clone(),
            &[(
                LogicalTarget::ClaudeSettings,
                Some(r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"do-not-log"}}"#),
            )],
        );
        let debug = format!("{candidates:?}");
        assert!(!debug.contains("do-not-log"));
        assert!(debug.contains("<redacted>"));

        let desktop = ready(
            AppType::ClaudeDesktop,
            &[(
                LogicalTarget::ClaudeDesktopProfile,
                Some(
                    r#"{"inferenceGatewayBaseUrl":"https://example.com","inferenceGatewayApiKey":"do-not-log","inferenceModels":[{"name":"claude-sonnet-private-model","labelOverride":"private-label"}]}"#,
                ),
            )],
        );
        let desktop_debug = format!("{desktop:?}");
        assert!(!desktop_debug.contains("do-not-log"));
        assert!(!desktop_debug.contains("private-model"));
        assert!(!desktop_debug.contains("private-label"));
        assert!(desktop_debug.contains("route_count: 1"));

        assert!(matches!(
            project_native_import(
                &AppType::Codex,
                &documents(AppType::Claude, &[(LogicalTarget::ClaudeSettings, None)])
            ),
            Err(NativeImportError::WrongDocumentApp { .. })
        ));
    }
}
