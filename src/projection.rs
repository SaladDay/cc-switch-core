//! Pure native live-configuration plan projection.
//!
//! Consumers normalize product state into this module's request types and
//! retain ownership of paths, file I/O, locking, execution, and rollback.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    io::{self, Write},
};

use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::{
    claude, claude_desktop, codex, common_config, gemini, grokbuild, hermes, openclaw, opencode,
    pi, AppType, ContentExpectation, LiveDocumentSet, LogicalTarget, OperationPlan,
    OperationPlanError, PlannedWrite, ProviderEntry, ProviderSnapshot, MAX_OPERATION_CONTENT_BYTES,
    OPERATION_CONTRACT_MAJOR,
};

const CLAUDE_DESKTOP_PROFILE_ID: &str = "00000000-0000-4000-8000-000000157210";
const CLAUDE_DESKTOP_PROFILE_NAME: &str = "CC Switch";
const OPENCLAW_DEFAULT_SOURCE: &str =
    "{\n  models: {\n    mode: 'merge',\n    providers: {},\n  },\n}\n";

/// Maximum encoded bytes accepted for each non-document plan input.
pub const MAX_NATIVE_PLAN_INPUT_BYTES: usize = MAX_OPERATION_CONTENT_BYTES;

/// Maximum number of typed Claude Desktop model routes accepted in one plan.
pub const MAX_NATIVE_PLAN_ROUTES: usize = 4096;

/// Native operation requested by a consumer-owned host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAction {
    Apply,
    Remove,
}

/// Normalized provider mode independent of a consumer's storage schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeProviderMode {
    Official,
    Custom,
}

/// Whether the consumer permits this provider to change native files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeProviderAccess {
    Writable,
    ReadOnly,
}

/// App-specific context normalized by the consumer.
pub enum NativePlanContext<'a> {
    /// Context for every app except Claude Desktop.
    Standard { common_config: Option<&'a str> },
    /// Typed direct routes for Claude Desktop. Official mode ignores routes.
    ClaudeDesktop {
        routes: &'a [claude_desktop::DirectModelRoute],
    },
}

impl fmt::Debug for NativePlanContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard { common_config } => formatter
                .debug_struct("Standard")
                .field("common_config", &common_config.map(|_| "<redacted>"))
                .finish(),
            Self::ClaudeDesktop { routes } => formatter
                .debug_struct("ClaudeDesktop")
                .field("route_count", &routes.len())
                .finish(),
        }
    }
}

/// Complete input for one shared native-plan projection.
pub struct NativePlanRequest<'a> {
    pub action: NativeAction,
    pub provider: &'a ProviderSnapshot,
    pub documents: &'a LiveDocumentSet,
    pub mode: NativeProviderMode,
    pub access: NativeProviderAccess,
    pub context: NativePlanContext<'a>,
}

impl fmt::Debug for NativePlanRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativePlanRequest")
            .field("action", &self.action)
            .field("provider", &self.provider)
            .field("documents", &self.documents)
            .field("mode", &self.mode)
            .field("access", &self.access)
            .field("context", &self.context)
            .finish()
    }
}

/// Rejection reason while projecting a native operation plan.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NativePlanError {
    #[error("provider belongs to '{actual}', expected '{expected}'")]
    WrongProviderApp { expected: String, actual: String },
    #[error("live documents belong to '{actual}', expected '{expected}'")]
    WrongDocumentApp { expected: String, actual: String },
    #[error("provider '{provider_id}' is read-only")]
    ReadOnlyProvider { provider_id: String },
    #[error("application '{app_id}' does not support native action {action:?}")]
    UnsupportedAction {
        app_id: String,
        action: NativeAction,
    },
    #[error("native plan context is invalid for '{app_id}': {message}")]
    InvalidContext { app_id: String, message: String },
    #[error("provider configuration is invalid for '{app_id}': {message}")]
    InvalidProvider { app_id: String, message: String },
    #[error("live document {target:?} is invalid: {message}")]
    InvalidDocument {
        target: LogicalTarget,
        message: String,
    },
    #[error("native plan {field} exceeds the {limit}-byte input limit")]
    InputTooLarge { field: &'static str, limit: usize },
    #[error("native plan route count exceeds the {limit}-route input limit")]
    TooManyRoutes { limit: usize },
    #[error(transparent)]
    InvalidPlan(#[from] OperationPlanError),
}

pub(crate) fn required_native_targets(
    adapter_app: &AppType,
    declared_targets: &[LogicalTarget],
    action: NativeAction,
    provider: &ProviderSnapshot,
    mode: NativeProviderMode,
) -> Result<Vec<LogicalTarget>, NativePlanError> {
    if provider.app != *adapter_app {
        return Err(NativePlanError::WrongProviderApp {
            expected: adapter_app.as_str().to_owned(),
            actual: provider.app.as_str().to_owned(),
        });
    }
    if action == NativeAction::Remove && !adapter_app.is_additive_mode() {
        return Err(NativePlanError::UnsupportedAction {
            app_id: adapter_app.as_str().to_owned(),
            action,
        });
    }

    let mut targets = declared_targets.to_vec();
    if *adapter_app == AppType::Codex && action == NativeAction::Apply {
        if mode == NativeProviderMode::Custom {
            targets.retain(|target| *target != LogicalTarget::CodexAuth);
        }
        if provider.settings.get("modelCatalog").is_none() {
            targets.retain(|target| *target != LogicalTarget::CodexModelCatalog);
        }
    }
    Ok(targets)
}

pub(crate) fn plan_native(
    adapter_app: &AppType,
    request: &NativePlanRequest<'_>,
) -> Result<OperationPlan, NativePlanError> {
    if request.provider.app != *adapter_app {
        return Err(NativePlanError::WrongProviderApp {
            expected: adapter_app.as_str().to_owned(),
            actual: request.provider.app.as_str().to_owned(),
        });
    }
    if request.documents.app() != adapter_app {
        return Err(NativePlanError::WrongDocumentApp {
            expected: adapter_app.as_str().to_owned(),
            actual: request.documents.app().as_str().to_owned(),
        });
    }
    validate_request_input(request)?;
    if request.access == NativeProviderAccess::ReadOnly {
        return Err(NativePlanError::ReadOnlyProvider {
            provider_id: request.provider.id.clone(),
        });
    }
    if request.action == NativeAction::Remove && !adapter_app.is_additive_mode() {
        return Err(NativePlanError::UnsupportedAction {
            app_id: adapter_app.as_str().to_owned(),
            action: NativeAction::Remove,
        });
    }
    match (adapter_app, &request.context) {
        (AppType::ClaudeDesktop, NativePlanContext::ClaudeDesktop { .. }) => {}
        (AppType::ClaudeDesktop, NativePlanContext::Standard { .. }) => {
            return Err(invalid_context(
                adapter_app,
                "Claude Desktop requires typed route context",
            ));
        }
        (_, NativePlanContext::Standard { .. }) => {}
        (_, NativePlanContext::ClaudeDesktop { .. }) => {
            return Err(invalid_context(
                adapter_app,
                "Claude Desktop route context belongs to another application",
            ));
        }
    }

    match request.action {
        NativeAction::Apply => prepare_apply(request),
        NativeAction::Remove => prepare_remove(request),
    }
}

fn validate_request_input(request: &NativePlanRequest<'_>) -> Result<(), NativePlanError> {
    ensure_input_size("provider id", request.provider.id.len())?;
    if request.action == NativeAction::Remove {
        return Ok(());
    }
    ensure_input_size("provider name", request.provider.name.len())?;

    let mut writer = SizeLimitedWriter::new(MAX_NATIVE_PLAN_INPUT_BYTES);
    if serde_json::to_writer(&mut writer, &request.provider.settings).is_err() {
        if writer.exceeded {
            return Err(input_too_large("provider settings"));
        }
        return Err(invalid_provider(
            &request.provider.app,
            "provider settings could not be serialized",
        ));
    }

    match request.context {
        NativePlanContext::Standard {
            common_config: Some(common_config),
        } => ensure_input_size("common configuration", common_config.len()),
        NativePlanContext::Standard {
            common_config: None,
        } => Ok(()),
        NativePlanContext::ClaudeDesktop { routes } => {
            if routes.len() > MAX_NATIVE_PLAN_ROUTES {
                return Err(NativePlanError::TooManyRoutes {
                    limit: MAX_NATIVE_PLAN_ROUTES,
                });
            }
            let bytes = routes.iter().fold(0usize, |total, route| {
                total
                    .saturating_add(route.route_id.len())
                    .saturating_add(route.upstream_model.len())
                    .saturating_add(route.label_override.as_deref().map_or(0, str::len))
            });
            ensure_input_size("Claude Desktop routes", bytes)
        }
    }
}

fn ensure_input_size(field: &'static str, bytes: usize) -> Result<(), NativePlanError> {
    if bytes > MAX_NATIVE_PLAN_INPUT_BYTES {
        Err(input_too_large(field))
    } else {
        Ok(())
    }
}

fn input_too_large(field: &'static str) -> NativePlanError {
    NativePlanError::InputTooLarge {
        field,
        limit: MAX_NATIVE_PLAN_INPUT_BYTES,
    }
}

struct SizeLimitedWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl SizeLimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for SizeLimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(io::Error::other("size limit exceeded"));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn prepare_apply(request: &NativePlanRequest<'_>) -> Result<OperationPlan, NativePlanError> {
    let app = &request.provider.app;
    let settings = apply_common_config(request)?;
    match app {
        AppType::Claude => {
            let snapshot = claude::prepare_live_snapshot(&settings)
                .map_err(|error| invalid_provider(app, error.to_string()))?;
            single_write(
                app,
                request.documents,
                LogicalTarget::ClaudeSettings,
                pretty_json(&snapshot.settings, LogicalTarget::ClaudeSettings)?,
            )
        }
        AppType::Codex => codex_plan(request, &settings),
        AppType::Gemini => gemini_plan(request, &settings),
        AppType::GrokBuild => grokbuild_plan(request, &settings),
        AppType::OpenCode => {
            require_custom_mode(request)?;
            let entry = opencode::prepare_provider_entry(&request.provider.id, &settings)
                .map_err(|error| invalid_provider(app, error.to_string()))?;
            json_entry_plan(request, entry, &["provider"], JsonRoot::Empty)
        }
        AppType::OpenClaw => {
            require_custom_mode(request)?;
            let entry = openclaw::prepare_provider_entry(&request.provider.id, &settings)
                .map_err(|error| invalid_provider(app, error.to_string()))?;
            json_entry_plan(request, entry, &["models", "providers"], JsonRoot::OpenClaw)
        }
        AppType::ClaudeDesktop => claude_desktop_plan(request, &settings),
        AppType::Hermes => {
            require_custom_mode(request)?;
            hermes_plan(request, &settings)
        }
        AppType::Pi => {
            require_custom_mode(request)?;
            let entry = pi::prepare_provider_entry(&request.provider.id, &settings)
                .map_err(|error| invalid_provider(app, error.to_string()))?;
            json_entry_plan(request, entry, &["providers"], JsonRoot::Empty)
        }
    }
}

fn prepare_remove(request: &NativePlanRequest<'_>) -> Result<OperationPlan, NativePlanError> {
    require_custom_mode(request)?;
    match request.provider.app {
        AppType::OpenCode => json_remove_plan(request, &["provider"], JsonRoot::Empty),
        AppType::OpenClaw => {
            json_remove_plan(request, &["models", "providers"], JsonRoot::OpenClaw)
        }
        AppType::Hermes => hermes_remove_plan(request),
        AppType::Pi => json_remove_plan(request, &["providers"], JsonRoot::Empty),
        _ => Err(NativePlanError::UnsupportedAction {
            app_id: request.provider.app.as_str().to_owned(),
            action: NativeAction::Remove,
        }),
    }
}

fn apply_common_config(request: &NativePlanRequest<'_>) -> Result<Value, NativePlanError> {
    let common = match request.context {
        NativePlanContext::Standard { common_config } => common_config,
        NativePlanContext::ClaudeDesktop { .. } => None,
    };
    common_config::apply(
        &request.provider.app,
        &request.provider.settings,
        common,
        common.is_some(),
    )
    .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))
}

fn require_custom_mode(request: &NativePlanRequest<'_>) -> Result<(), NativePlanError> {
    if request.mode == NativeProviderMode::Custom {
        Ok(())
    } else {
        Err(invalid_context(
            &request.provider.app,
            "this application does not define an official native provider mode",
        ))
    }
}

fn codex_plan(
    request: &NativePlanRequest<'_>,
    settings: &Value,
) -> Result<OperationPlan, NativePlanError> {
    let snapshot = codex::prepare_strict_live_snapshot(settings)
        .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let config_original = contents(request.documents, LogicalTarget::CodexConfig)?;
    let config = codex::prepare_provider_live_config(
        &snapshot.auth,
        snapshot.config.as_deref().unwrap_or_default(),
    )
    .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let catalog = codex::prepare_native_model_catalog(
        settings,
        &config,
        codex::NativeCatalogOwnership::default(),
    )
    .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let catalog_managed = catalog.managed;
    let config =
        preserve_live_mcp_toml(config_original, &catalog.config, LogicalTarget::CodexConfig)?;
    let mut writes = Vec::with_capacity(3);
    let official = request.mode == NativeProviderMode::Official;
    if official {
        let auth_original = contents(request.documents, LogicalTarget::CodexAuth)?;
        if codex::should_write_auth(Some("official"), &snapshot.auth, true) {
            writes.push(planned(
                LogicalTarget::CodexAuth,
                auth_original,
                Some(pretty_json(&snapshot.auth, LogicalTarget::CodexAuth)?),
            ));
        } else if parse_optional_json_object(auth_original, LogicalTarget::CodexAuth)?
            .as_ref()
            .is_some_and(codex::live_auth_is_stale_third_party_residue)
        {
            writes.push(planned(LogicalTarget::CodexAuth, auth_original, None));
        }
    }
    writes.push(planned(
        LogicalTarget::CodexConfig,
        config_original,
        Some(config),
    ));
    if catalog_managed {
        let original = contents(request.documents, LogicalTarget::CodexModelCatalog)?;
        writes.push(planned(
            LogicalTarget::CodexModelCatalog,
            original,
            catalog
                .catalog
                .as_ref()
                .map(|value| pretty_json(value, LogicalTarget::CodexModelCatalog))
                .transpose()?,
        ));
    }
    plan(&request.provider.app, writes)
}

fn gemini_plan(
    request: &NativePlanRequest<'_>,
    settings: &Value,
) -> Result<OperationPlan, NativePlanError> {
    let env_original = contents(request.documents, LogicalTarget::GeminiEnv)?;
    let settings_original = contents(request.documents, LogicalTarget::GeminiSettings)?;
    let existing = parse_optional_json_object(settings_original, LogicalTarget::GeminiSettings)?;
    let mode = if request.mode == NativeProviderMode::Official {
        gemini::AuthMode::OAuthPersonal
    } else {
        gemini::AuthMode::ApiKey
    };
    let snapshot = gemini::prepare_live_snapshot(settings, existing.as_ref(), mode)
        .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    plan(
        &request.provider.app,
        vec![
            planned(
                LogicalTarget::GeminiEnv,
                env_original,
                Some(serialize_env(&snapshot.env)?),
            ),
            planned(
                LogicalTarget::GeminiSettings,
                settings_original,
                Some(pretty_json(
                    &snapshot.settings,
                    LogicalTarget::GeminiSettings,
                )?),
            ),
        ],
    )
}

fn grokbuild_plan(
    request: &NativePlanRequest<'_>,
    settings: &Value,
) -> Result<OperationPlan, NativePlanError> {
    let mode = if request.mode == NativeProviderMode::Official {
        grokbuild::ProviderMode::Official
    } else {
        grokbuild::ProviderMode::Custom
    };
    let snapshot = grokbuild::prepare_live_snapshot(settings, mode)
        .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let original = contents(request.documents, LogicalTarget::GrokConfig)?;
    let config = preserve_live_mcp_toml(original, &snapshot.config, LogicalTarget::GrokConfig)?;
    plan(
        &request.provider.app,
        vec![planned(LogicalTarget::GrokConfig, original, Some(config))],
    )
}

fn claude_desktop_plan(
    request: &NativePlanRequest<'_>,
    settings: &Value,
) -> Result<OperationPlan, NativePlanError> {
    let NativePlanContext::ClaudeDesktop { routes } = request.context else {
        return Err(invalid_context(
            &request.provider.app,
            "Claude Desktop requires typed route context",
        ));
    };
    let official = request.mode == NativeProviderMode::Official;
    let action = claude_desktop::prepare_live_action(
        settings,
        if official {
            claude_desktop::ProviderMode::Official
        } else {
            claude_desktop::ProviderMode::Direct
        },
        Some(routes),
    )
    .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let normal_original = contents(request.documents, LogicalTarget::ClaudeDesktopNormalConfig)?;
    let threep_original = contents(request.documents, LogicalTarget::ClaudeDesktopThreepConfig)?;
    let profile_original = contents(request.documents, LogicalTarget::ClaudeDesktopProfile)?;
    let meta_original = contents(request.documents, LogicalTarget::ClaudeDesktopMeta)?;
    let mut normal =
        parse_json_object_or_empty(normal_original, LogicalTarget::ClaudeDesktopNormalConfig)?;
    let mut threep =
        parse_json_object_or_empty(threep_original, LogicalTarget::ClaudeDesktopThreepConfig)?;
    let mut meta = parse_json_object_or_empty(meta_original, LogicalTarget::ClaudeDesktopMeta)?;
    let mut writes = Vec::with_capacity(4);

    match action {
        claude_desktop::PreparedLiveAction::RestoreOfficial => {
            normal.insert("deploymentMode".to_owned(), json!("1p"));
            threep.insert("deploymentMode".to_owned(), json!("1p"));
            remove_desktop_enterprise_config(&mut threep);
            update_desktop_meta(&mut meta, false);
            writes.push(planned(
                LogicalTarget::ClaudeDesktopProfile,
                profile_original,
                None,
            ));
        }
        claude_desktop::PreparedLiveAction::ApplyDirect { profile } => {
            normal.insert("deploymentMode".to_owned(), json!("3p"));
            threep.insert("deploymentMode".to_owned(), json!("3p"));
            update_desktop_meta(&mut meta, true);
            writes.push(planned(
                LogicalTarget::ClaudeDesktopProfile,
                profile_original,
                Some(pretty_json(&profile, LogicalTarget::ClaudeDesktopProfile)?),
            ));
        }
    }
    writes.push(planned(
        LogicalTarget::ClaudeDesktopNormalConfig,
        normal_original,
        Some(pretty_json(
            &Value::Object(normal),
            LogicalTarget::ClaudeDesktopNormalConfig,
        )?),
    ));
    writes.push(planned(
        LogicalTarget::ClaudeDesktopThreepConfig,
        threep_original,
        Some(pretty_json(
            &Value::Object(threep),
            LogicalTarget::ClaudeDesktopThreepConfig,
        )?),
    ));
    writes.push(planned(
        LogicalTarget::ClaudeDesktopMeta,
        meta_original,
        Some(pretty_json(
            &Value::Object(meta),
            LogicalTarget::ClaudeDesktopMeta,
        )?),
    ));
    plan(&request.provider.app, writes)
}

fn single_write(
    app: &AppType,
    documents: &LiveDocumentSet,
    target: LogicalTarget,
    next: String,
) -> Result<OperationPlan, NativePlanError> {
    let original = contents(documents, target)?;
    plan(app, vec![planned(target, original, Some(next))])
}

fn plan(app: &AppType, writes: Vec<PlannedWrite>) -> Result<OperationPlan, NativePlanError> {
    Ok(OperationPlan {
        contract_major: OPERATION_CONTRACT_MAJOR,
        app_id: app.as_str().to_owned(),
        writes,
    })
}

fn planned(
    target: LogicalTarget,
    original: Option<&[u8]>,
    contents: Option<String>,
) -> PlannedWrite {
    PlannedWrite {
        target,
        expected: ContentExpectation::for_contents(original),
        contents,
    }
}

fn contents(
    documents: &LiveDocumentSet,
    target: LogicalTarget,
) -> Result<Option<&[u8]>, NativePlanError> {
    let document = documents
        .document(target)
        .ok_or_else(|| invalid_document(target, "target was not supplied by the host"))?;
    if !document.is_observed() {
        return Err(invalid_document(
            target,
            "target was not observed by the host",
        ));
    }
    Ok(document.contents())
}

fn invalid_context(app: &AppType, message: impl Into<String>) -> NativePlanError {
    NativePlanError::InvalidContext {
        app_id: app.as_str().to_owned(),
        message: message.into(),
    }
}

fn invalid_provider(app: &AppType, message: impl Into<String>) -> NativePlanError {
    NativePlanError::InvalidProvider {
        app_id: app.as_str().to_owned(),
        message: message.into(),
    }
}

fn invalid_document(target: LogicalTarget, message: impl Into<String>) -> NativePlanError {
    NativePlanError::InvalidDocument {
        target,
        message: message.into(),
    }
}

fn json_entry_plan(
    request: &NativePlanRequest<'_>,
    entry: ProviderEntry,
    keys: &[&str],
    default: JsonRoot,
) -> Result<OperationPlan, NativePlanError> {
    let target = json_target(&request.provider.app)?;
    let original = contents(request.documents, target)?;
    let mut root = parse_json5_object(original, target, default.value())?;
    ensure_nested_object(&mut root, keys, target)?.insert(entry.key, entry.config);
    let next = serialize_json_root(&request.provider.app, &root, original)?;
    plan(
        &request.provider.app,
        vec![planned(target, original, Some(next))],
    )
}

fn json_remove_plan(
    request: &NativePlanRequest<'_>,
    keys: &[&str],
    default: JsonRoot,
) -> Result<OperationPlan, NativePlanError> {
    let target = json_target(&request.provider.app)?;
    let original = contents(request.documents, target)?;
    let mut root = parse_json5_object(original, target, default.value())?;
    if let Some(entries) = nested_object_mut(&mut root, keys) {
        entries.remove(&request.provider.id);
    }
    let next = serialize_json_root(&request.provider.app, &root, original)?;
    plan(
        &request.provider.app,
        vec![planned(target, original, Some(next))],
    )
}

fn hermes_plan(
    request: &NativePlanRequest<'_>,
    settings: &Value,
) -> Result<OperationPlan, NativePlanError> {
    let entry = hermes::prepare_provider_entry(&request.provider.id, settings)
        .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let target = LogicalTarget::HermesConfig;
    let original = contents(request.documents, target)?;
    let raw = optional_utf8(original, target)?;
    let root = parse_yaml(raw, target)?;
    if hermes_dict_only(&root, &request.provider.id) {
        return Err(invalid_provider(
            &request.provider.app,
            "provider is managed by the native providers dictionary",
        ));
    }
    let mut providers = root
        .get("custom_providers")
        .and_then(serde_yaml::Value::as_sequence)
        .cloned()
        .unwrap_or_default();
    let mut next = json_to_yaml(&entry.config, target)?;
    if let Some(existing) = providers.iter_mut().find(|value| {
        value.get("name").and_then(serde_yaml::Value::as_str) == Some(request.provider.id.as_str())
    }) {
        if let (Some(existing), Some(next)) = (existing.as_mapping(), next.as_mapping_mut()) {
            for (key, value) in existing {
                next.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        *existing = next;
    } else {
        providers.push(next);
    }
    let next = replace_yaml_section(
        raw,
        "custom_providers",
        &serde_yaml::Value::Sequence(providers),
        target,
    )?;
    let current_model = root.get("model").map(yaml_to_json).transpose()?;
    let model =
        hermes::prepare_model_defaults(&request.provider.id, settings, current_model.as_ref())
            .map_err(|error| invalid_provider(&request.provider.app, error.to_string()))?;
    let model = json_to_yaml(&model, target)?;
    let next = replace_yaml_section(&next, "model", &model, target)?;
    plan(
        &request.provider.app,
        vec![planned(target, original, Some(next))],
    )
}

fn hermes_remove_plan(request: &NativePlanRequest<'_>) -> Result<OperationPlan, NativePlanError> {
    let target = LogicalTarget::HermesConfig;
    let original = contents(request.documents, target)?;
    let raw = optional_utf8(original, target)?;
    let root = parse_yaml(raw, target)?;
    if hermes_dict_only(&root, &request.provider.id) {
        return Err(invalid_provider(
            &request.provider.app,
            "provider is managed by the native providers dictionary",
        ));
    }
    let providers = root
        .get("custom_providers")
        .and_then(serde_yaml::Value::as_sequence)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|value| {
            value.get("name").and_then(serde_yaml::Value::as_str)
                != Some(request.provider.id.as_str())
        })
        .collect();
    let next = replace_yaml_section(
        raw,
        "custom_providers",
        &serde_yaml::Value::Sequence(providers),
        target,
    )?;
    let next = match root.get("model").and_then(serde_yaml::Value::as_mapping) {
        Some(model)
            if model.get("provider").and_then(serde_yaml::Value::as_str)
                == Some(request.provider.id.as_str()) =>
        {
            let mut model = model.clone();
            model.remove("provider");
            model.remove("default");
            replace_yaml_section(&next, "model", &serde_yaml::Value::Mapping(model), target)?
        }
        _ => next,
    };
    plan(
        &request.provider.app,
        vec![planned(target, original, Some(next))],
    )
}

fn optional_utf8(contents: Option<&[u8]>, target: LogicalTarget) -> Result<&str, NativePlanError> {
    contents
        .map(|contents| {
            std::str::from_utf8(contents)
                .map_err(|_| invalid_document(target, "contents are not UTF-8"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn parse_json_value(contents: &[u8], target: LogicalTarget) -> Result<Value, NativePlanError> {
    serde_json::from_slice(contents)
        .map_err(|_| invalid_document(target, "JSON could not be parsed"))
}

fn parse_json5_object(
    contents: Option<&[u8]>,
    target: LogicalTarget,
    default: Value,
) -> Result<Map<String, Value>, NativePlanError> {
    let Some(contents) = contents else {
        return default
            .as_object()
            .cloned()
            .ok_or_else(|| invalid_document(target, "default root is not an object"));
    };
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid_document(target, "contents are not UTF-8"))?;
    let value: Value =
        json5::from_str(text).map_err(|_| invalid_document(target, "JSON5 could not be parsed"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_document(target, "root must be an object"))
}

fn parse_json_object_or_empty(
    contents: Option<&[u8]>,
    target: LogicalTarget,
) -> Result<Map<String, Value>, NativePlanError> {
    let Some(contents) = contents else {
        return Ok(Map::new());
    };
    parse_json_value(contents, target)?
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_document(target, "JSON root must be an object"))
}

fn parse_optional_json_object(
    contents: Option<&[u8]>,
    target: LogicalTarget,
) -> Result<Option<Value>, NativePlanError> {
    contents
        .map(|contents| parse_json_value(contents, target))
        .transpose()
        .and_then(|value| match value {
            Some(value) if value.is_object() => Ok(Some(value)),
            Some(_) => Err(invalid_document(target, "JSON root must be an object")),
            None => Ok(None),
        })
}

fn pretty_json(value: &Value, target: LogicalTarget) -> Result<String, NativePlanError> {
    let mut contents = serde_json::to_string_pretty(value)
        .map_err(|_| invalid_document(target, "JSON could not be serialized"))?;
    contents.push('\n');
    Ok(contents)
}

fn preserve_live_mcp_toml(
    original: Option<&[u8]>,
    provider_config: &str,
    target: LogicalTarget,
) -> Result<String, NativePlanError> {
    let mut next = provider_config
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid_provider(&target.app(), "provider TOML could not be parsed"))?;
    let current = original
        .map(|contents| {
            let contents = std::str::from_utf8(contents)
                .map_err(|_| invalid_document(target, "live TOML is not UTF-8"))?;
            contents
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| invalid_document(target, "live TOML could not be parsed"))
        })
        .transpose()?;

    next.as_table_mut().remove("mcp_servers");
    if let Some(mcp) = next
        .get_mut("mcp")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        mcp.remove("servers");
        if mcp.is_empty() {
            next.as_table_mut().remove("mcp");
        }
    }

    let Some(current) = current else {
        return Ok(next.to_string());
    };
    if let Some(servers) = current.get("mcp_servers") {
        next.as_table_mut().insert("mcp_servers", servers.clone());
    }
    if let Some(servers) = current
        .get("mcp")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|mcp| mcp.get("servers"))
    {
        if next.get("mcp").is_none() {
            next["mcp"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let mcp = next
            .get_mut("mcp")
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| invalid_provider(&target.app(), "provider mcp field must be a table"))?;
        mcp.insert("servers", servers.clone());
    }
    Ok(next.to_string())
}

fn serialize_env(env: &BTreeMap<String, String>) -> Result<String, NativePlanError> {
    let mut lines = Vec::with_capacity(env.len());
    for (key, value) in env {
        if !valid_env_key(key) || value.contains(['\r', '\n', '\0']) {
            return Err(invalid_provider(
                &AppType::Gemini,
                "environment contains an unsafe key or value",
            ));
        }
        lines.push(format!("{key}={value}"));
    }
    Ok(lines.join("\n"))
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn nested_object_mut<'a>(
    root: &'a mut Map<String, Value>,
    keys: &[&str],
) -> Option<&'a mut Map<String, Value>> {
    let mut current = root;
    for key in keys {
        current = current.get_mut(*key)?.as_object_mut()?;
    }
    Some(current)
}

fn ensure_nested_object<'a>(
    root: &'a mut Map<String, Value>,
    keys: &[&str],
    target: LogicalTarget,
) -> Result<&'a mut Map<String, Value>, NativePlanError> {
    let mut current = root;
    for key in keys {
        let value = current
            .entry((*key).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        current = value
            .as_object_mut()
            .ok_or_else(|| invalid_document(target, "provider container must be an object"))?;
    }
    Ok(current)
}

enum JsonRoot {
    Empty,
    OpenClaw,
}

impl JsonRoot {
    fn value(&self) -> Value {
        match self {
            Self::Empty => json!({}),
            Self::OpenClaw => json!({"models": {"mode": "merge", "providers": {}}}),
        }
    }
}

fn serialize_json_root(
    app: &AppType,
    root: &Map<String, Value>,
    original: Option<&[u8]>,
) -> Result<String, NativePlanError> {
    if *app != AppType::OpenClaw {
        return pretty_json(&Value::Object(root.clone()), json_target(app)?);
    }
    let target = LogicalTarget::OpenClawConfig;
    let source = optional_utf8(original, target)?;
    let source = if source.trim().is_empty() {
        OPENCLAW_DEFAULT_SOURCE
    } else {
        source
    };
    let models = root
        .get("models")
        .ok_or_else(|| invalid_document(target, "models section is missing"))?;
    replace_json5_root_section(source, "models", models, target)
}

fn replace_json5_root_section(
    source: &str,
    key: &str,
    value: &Value,
    target: LogicalTarget,
) -> Result<String, NativePlanError> {
    crate::json5_patch::replace_top_level_value(source, key, value)
        .map_err(|message| invalid_document(target, message))
}

fn json_target(app: &AppType) -> Result<LogicalTarget, NativePlanError> {
    match app {
        AppType::OpenCode => Ok(LogicalTarget::OpenCodeConfig),
        AppType::OpenClaw => Ok(LogicalTarget::OpenClawConfig),
        AppType::Pi => Ok(LogicalTarget::PiModels),
        _ => Err(invalid_context(
            app,
            "application is not a JSON additive target",
        )),
    }
}

fn remove_desktop_enterprise_config(root: &mut Map<String, Value>) {
    let Some(enterprise) = root
        .get_mut("enterpriseConfig")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for key in [
        "disableDeploymentModeChooser",
        "inferenceGatewayApiKey",
        "inferenceGatewayAuthScheme",
        "inferenceGatewayBaseUrl",
        "inferenceProvider",
    ] {
        enterprise.remove(key);
    }
    if enterprise.is_empty() {
        root.remove("enterpriseConfig");
    }
}

fn update_desktop_meta(root: &mut Map<String, Value>, apply: bool) {
    let mut entries = root
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    entries
        .retain(|entry| entry.get("id").and_then(Value::as_str) != Some(CLAUDE_DESKTOP_PROFILE_ID));
    if apply {
        entries.push(json!({
            "id": CLAUDE_DESKTOP_PROFILE_ID,
            "name": CLAUDE_DESKTOP_PROFILE_NAME
        }));
        root.insert("appliedId".to_owned(), json!(CLAUDE_DESKTOP_PROFILE_ID));
    } else if root.get("appliedId").and_then(Value::as_str) == Some(CLAUDE_DESKTOP_PROFILE_ID) {
        match entries
            .iter()
            .find_map(|entry| entry.get("id").and_then(Value::as_str))
        {
            Some(id) => {
                root.insert("appliedId".to_owned(), json!(id));
            }
            None => {
                root.remove("appliedId");
            }
        }
    }
    root.insert("entries".to_owned(), Value::Array(entries));
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

fn parse_yaml(contents: &str, target: LogicalTarget) -> Result<serde_yaml::Value, NativePlanError> {
    if contents.trim().is_empty() {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    if let Some(key) = duplicate_yaml_top_level_key(contents) {
        return Err(invalid_document(
            target,
            format!("YAML contains duplicate top-level key '{key}'"),
        ));
    }
    let value: serde_yaml::Value = serde_yaml::from_str(contents)
        .map_err(|_| invalid_document(target, "YAML could not be parsed"))?;
    if !value.is_mapping() {
        return Err(invalid_document(target, "YAML root must be a mapping"));
    }
    Ok(value)
}

fn yaml_to_json(value: &serde_yaml::Value) -> Result<Value, NativePlanError> {
    serde_json::to_value(value).map_err(|_| {
        invalid_document(
            LogicalTarget::HermesConfig,
            "provider is not JSON-compatible",
        )
    })
}

fn json_to_yaml(
    value: &Value,
    target: LogicalTarget,
) -> Result<serde_yaml::Value, NativePlanError> {
    serde_yaml::to_value(value)
        .map_err(|_| invalid_document(target, "provider could not be serialized as YAML"))
}

fn hermes_dict_only(root: &serde_yaml::Value, name: &str) -> bool {
    let list_has = root
        .get("custom_providers")
        .and_then(serde_yaml::Value::as_sequence)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.get("name").and_then(serde_yaml::Value::as_str) == Some(name))
        });
    !list_has
        && root
            .get("providers")
            .and_then(serde_yaml::Value::as_mapping)
            .is_some_and(|entries| {
                entries.iter().any(|(key, value)| {
                    key.as_str() == Some(name)
                        || value.get("name").and_then(serde_yaml::Value::as_str) == Some(name)
                })
            })
}

fn replace_yaml_section(
    raw: &str,
    key: &str,
    value: &serde_yaml::Value,
    target: LogicalTarget,
) -> Result<String, NativePlanError> {
    let mut section = serde_yaml::Mapping::new();
    section.insert(serde_yaml::Value::String(key.to_owned()), value.clone());
    let serialized = serde_yaml::to_string(&serde_yaml::Value::Mapping(section))
        .map_err(|_| invalid_document(target, "YAML could not be serialized"))?;
    let Some((start, end)) = yaml_section_range(raw, key) else {
        let mut result = raw.to_owned();
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&serialized);
        return Ok(result);
    };
    let mut result = String::with_capacity(raw.len() + serialized.len());
    result.push_str(&raw[..start]);
    result.push_str(&serialized);
    result.push_str(&remove_yaml_sections(&raw[end..], key));
    Ok(result)
}

fn remove_yaml_sections(raw: &str, key: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut remaining = raw;
    while let Some((start, end)) = yaml_section_range(remaining, key) {
        result.push_str(&remaining[..start]);
        remaining = &remaining[end..];
    }
    result.push_str(remaining);
    result
}

fn yaml_section_range(raw: &str, key: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut offset = 0;
    for line in raw.split('\n') {
        let top_level = !line.is_empty()
            && !line.starts_with([' ', '\t', '#', '-'])
            && line.find(':').is_some_and(|index| {
                let suffix = &line[index + 1..];
                suffix.is_empty() || suffix.starts_with([' ', '\t', '\r'])
            });
        if start.is_none() && top_level && yaml_key_matches(line, key) {
            start = Some(offset);
        } else if let (Some(start), true) = (start, top_level) {
            return Some((start, offset));
        }
        offset += line.len() + 1;
    }
    start.map(|start| (start, raw.len()))
}

fn yaml_key_matches(line: &str, key: &str) -> bool {
    let quoted = format!("\"{key}\"");
    let single_quoted = format!("'{key}'");
    let matches = [key, quoted.as_str(), single_quoted.as_str()]
        .into_iter()
        .any(|candidate| {
            line.strip_prefix(candidate)
                .map(str::trim_start)
                .and_then(|suffix| suffix.strip_prefix(':'))
                .is_some_and(|suffix| {
                    suffix.is_empty() || suffix.chars().next().is_some_and(char::is_whitespace)
                })
        });
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin_app_adapter, ObservedDocument};

    fn document_set(app: AppType, present: &[(LogicalTarget, &str)]) -> LiveDocumentSet {
        let observations = builtin_app_adapter(&app)
            .targets()
            .iter()
            .copied()
            .map(|target| {
                present
                    .iter()
                    .find(|(candidate, _)| *candidate == target)
                    .map_or_else(
                        || ObservedDocument::missing(target),
                        |(_, contents)| ObservedDocument::present(target, contents.as_bytes()),
                    )
            });
        LiveDocumentSet::try_new(app, observations).expect("complete document set")
    }

    fn standard_plan(
        app: AppType,
        action: NativeAction,
        id: &str,
        settings: Value,
        mode: NativeProviderMode,
        documents: &LiveDocumentSet,
        common_config: Option<&str>,
    ) -> Result<OperationPlan, NativePlanError> {
        let provider = ProviderSnapshot::new(id, app.clone(), id, settings);
        builtin_app_adapter(&app).plan_native(&NativePlanRequest {
            action,
            provider: &provider,
            documents,
            mode,
            access: NativeProviderAccess::Writable,
            context: NativePlanContext::Standard { common_config },
        })
    }

    fn write_contents(plan: &OperationPlan, target: LogicalTarget) -> &str {
        plan.writes
            .iter()
            .find(|write| write.target == target)
            .and_then(|write| write.contents.as_deref())
            .expect("target has a content write")
    }

    #[test]
    fn request_boundaries_are_typed_and_debug_output_is_redacted() {
        let documents = document_set(AppType::Claude, &[]);
        let provider = ProviderSnapshot::new(
            "secret-provider",
            AppType::Claude,
            "Secret Provider",
            json!({"env": {"ANTHROPIC_AUTH_TOKEN": "do-not-log"}}),
        );
        let request = NativePlanRequest {
            action: NativeAction::Apply,
            provider: &provider,
            documents: &documents,
            mode: NativeProviderMode::Custom,
            access: NativeProviderAccess::ReadOnly,
            context: NativePlanContext::Standard {
                common_config: Some("{\"permissions\":{\"allow\":[\"Read\"]}}"),
            },
        };

        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!debug.contains("permissions"));
        assert!(matches!(
            builtin_app_adapter(&AppType::Claude).plan_native(&request),
            Err(NativePlanError::ReadOnlyProvider { .. })
        ));

        let wrong_provider = ProviderSnapshot::new("codex", AppType::Codex, "Codex", json!({}));
        assert!(matches!(
            builtin_app_adapter(&AppType::Claude).plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &wrong_provider,
                documents: &documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::Standard {
                    common_config: None,
                },
            }),
            Err(NativePlanError::WrongProviderApp { .. })
        ));

        assert!(matches!(
            standard_plan(
                AppType::Claude,
                NativeAction::Remove,
                "claude",
                json!({}),
                NativeProviderMode::Custom,
                &documents,
                None,
            ),
            Err(NativePlanError::UnsupportedAction { .. })
        ));
    }

    #[test]
    fn request_size_limits_run_before_projection() {
        let claude_documents = document_set(AppType::Claude, &[]);
        let oversized = "x".repeat(MAX_NATIVE_PLAN_INPUT_BYTES + 1);

        assert!(matches!(
            standard_plan(
                AppType::Claude,
                NativeAction::Apply,
                &oversized,
                json!({}),
                NativeProviderMode::Custom,
                &claude_documents,
                None,
            ),
            Err(NativePlanError::InputTooLarge {
                field: "provider id",
                ..
            })
        ));

        let provider = ProviderSnapshot::new("claude", AppType::Claude, &oversized, json!({}));
        assert!(matches!(
            builtin_app_adapter(&AppType::Claude).plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &provider,
                documents: &claude_documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::Standard {
                    common_config: None,
                },
            }),
            Err(NativePlanError::InputTooLarge {
                field: "provider name",
                ..
            })
        ));

        assert!(matches!(
            standard_plan(
                AppType::Claude,
                NativeAction::Apply,
                "claude",
                json!({"oversized": oversized}),
                NativeProviderMode::Custom,
                &claude_documents,
                None,
            ),
            Err(NativePlanError::InputTooLarge {
                field: "provider settings",
                ..
            })
        ));

        assert!(matches!(
            standard_plan(
                AppType::Claude,
                NativeAction::Apply,
                "claude",
                json!({}),
                NativeProviderMode::Custom,
                &claude_documents,
                Some(&"x".repeat(MAX_NATIVE_PLAN_INPUT_BYTES + 1)),
            ),
            Err(NativePlanError::InputTooLarge {
                field: "common configuration",
                ..
            })
        ));

        let desktop_documents = document_set(AppType::ClaudeDesktop, &[]);
        let desktop =
            ProviderSnapshot::new("desktop", AppType::ClaudeDesktop, "Desktop", json!({}));
        let too_many_routes = vec![
            claude_desktop::DirectModelRoute {
                route_id: String::new(),
                upstream_model: String::new(),
                label_override: None,
                supports_1m: false,
            };
            MAX_NATIVE_PLAN_ROUTES + 1
        ];
        assert!(matches!(
            builtin_app_adapter(&AppType::ClaudeDesktop).plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &desktop,
                documents: &desktop_documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::ClaudeDesktop {
                    routes: &too_many_routes,
                },
            }),
            Err(NativePlanError::TooManyRoutes { .. })
        ));

        let oversized_route = [claude_desktop::DirectModelRoute {
            route_id: "x".repeat(MAX_NATIVE_PLAN_INPUT_BYTES + 1),
            upstream_model: String::new(),
            label_override: None,
            supports_1m: false,
        }];
        assert!(matches!(
            builtin_app_adapter(&AppType::ClaudeDesktop).plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &desktop,
                documents: &desktop_documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::ClaudeDesktop {
                    routes: &oversized_route,
                },
            }),
            Err(NativePlanError::InputTooLarge {
                field: "Claude Desktop routes",
                ..
            })
        ));

        let pi_documents = document_set(
            AppType::Pi,
            &[(
                LogicalTarget::PiModels,
                r#"{"providers":{"remove":{"models":[]}}}"#,
            )],
        );
        let removal = standard_plan(
            AppType::Pi,
            NativeAction::Remove,
            "remove",
            json!({"unused": "x".repeat(MAX_NATIVE_PLAN_INPUT_BYTES + 1)}),
            NativeProviderMode::Custom,
            &pi_documents,
            None,
        )
        .expect("removal only validates the provider id it consumes");
        let removed: Value =
            serde_json::from_str(write_contents(&removal, LogicalTarget::PiModels))
                .expect("Pi JSON");
        assert!(removed["providers"].get("remove").is_none());
    }

    #[test]
    fn claude_apply_projects_common_config_and_strips_internal_keys() {
        let documents = document_set(AppType::Claude, &[]);
        let plan = standard_plan(
            AppType::Claude,
            NativeAction::Apply,
            "claude",
            json!({
                "apiFormat": "internal",
                "env": {"ANTHROPIC_AUTH_TOKEN": "secret"},
                "nested": {"keep": true}
            }),
            NativeProviderMode::Custom,
            &documents,
            Some("{\"permissions\":{\"allow\":[\"Read\"]}}"),
        )
        .expect("valid Claude plan");

        builtin_app_adapter(&AppType::Claude)
            .validate_plan(&plan)
            .expect("adapter-owned plan");
        let value: Value =
            serde_json::from_str(write_contents(&plan, LogicalTarget::ClaudeSettings))
                .expect("Claude JSON");
        assert!(value.get("apiFormat").is_none());
        assert_eq!(value["nested"]["keep"], true);
        assert_eq!(value["permissions"]["allow"][0], "Read");
        assert_eq!(plan.writes[0].expected, ContentExpectation::Missing);
    }

    #[test]
    fn codex_apply_preserves_oauth_mcp_and_projects_owned_catalog() {
        let documents = document_set(
            AppType::Codex,
            &[
                (
                    LogicalTarget::CodexAuth,
                    r#"{"tokens":{"access_token":"oauth-login"}}"#,
                ),
                (
                    LogicalTarget::CodexConfig,
                    "model = \"old\"\n[mcp_servers.keep]\ncommand = \"keep\"\n",
                ),
            ],
        );
        let plan = standard_plan(
            AppType::Codex,
            NativeAction::Apply,
            "custom",
            json!({
                "auth": {"OPENAI_API_KEY": "provider-secret"},
                "config": "model = \"qwen3-coder-plus\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://example.com\"\n",
                "modelCatalog": {"models": [{"model": "qwen3-coder-plus"}]}
            }),
            NativeProviderMode::Custom,
            &documents,
            Some("[features]\nkeep = true\n"),
        )
        .expect("valid Codex plan");

        assert!(plan
            .writes
            .iter()
            .all(|write| write.target != LogicalTarget::CodexAuth));
        let config = write_contents(&plan, LogicalTarget::CodexConfig);
        assert!(config.contains("experimental_bearer_token = \"provider-secret\""));
        assert!(config.contains("[mcp_servers.keep]"));
        assert!(config.contains("[features]"));
        assert!(config.contains("model_catalog_json = \"cc-switch-model-catalog.json\""));
        let catalog: Value =
            serde_json::from_str(write_contents(&plan, LogicalTarget::CodexModelCatalog))
                .expect("Codex catalog JSON");
        assert_eq!(catalog["models"][0]["slug"], "qwen3-coder-plus");
    }

    #[test]
    fn codex_requires_the_catalog_target_only_when_the_provider_manages_it() {
        let adapter = builtin_app_adapter(&AppType::Codex);
        let unmanaged = ProviderSnapshot::new(
            "custom",
            AppType::Codex,
            "Custom",
            json!({"auth": {}, "config": "model = \"gpt-5\"\n"}),
        );
        let managed = ProviderSnapshot::new(
            "custom",
            AppType::Codex,
            "Custom",
            json!({
                "auth": {},
                "config": "model = \"gpt-5\"\n",
                "modelCatalog": {"models": []}
            }),
        );

        let unmanaged_targets = adapter
            .required_native_targets(NativeAction::Apply, &unmanaged, NativeProviderMode::Custom)
            .expect("unmanaged target set");
        assert!(!unmanaged_targets.contains(&LogicalTarget::CodexAuth));
        assert!(!unmanaged_targets.contains(&LogicalTarget::CodexModelCatalog));
        let managed_targets = adapter
            .required_native_targets(NativeAction::Apply, &managed, NativeProviderMode::Custom)
            .expect("managed target set");
        assert!(!managed_targets.contains(&LogicalTarget::CodexAuth));
        assert!(managed_targets.contains(&LogicalTarget::CodexModelCatalog));
        let official_targets = adapter
            .required_native_targets(
                NativeAction::Apply,
                &unmanaged,
                NativeProviderMode::Official,
            )
            .expect("official target set");
        assert!(official_targets.contains(&LogicalTarget::CodexAuth));

        let documents = LiveDocumentSet::try_new(
            AppType::Codex,
            [
                ObservedDocument::unobserved(LogicalTarget::CodexAuth),
                ObservedDocument::missing(LogicalTarget::CodexConfig),
                ObservedDocument::unobserved(LogicalTarget::CodexModelCatalog),
            ],
        )
        .expect("complete Codex target inventory");
        adapter
            .plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &unmanaged,
                documents: &documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::Standard {
                    common_config: None,
                },
            })
            .expect("unmanaged catalog is not observed");
        assert!(matches!(
            adapter.plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &managed,
                documents: &documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::Standard {
                    common_config: None,
                },
            }),
            Err(NativePlanError::InvalidDocument {
                target: LogicalTarget::CodexModelCatalog,
                ..
            })
        ));
    }

    #[test]
    fn toml_1_1_mcp_tables_survive_codex_and_grok_projection() {
        let live_mcp =
            "[mcp_servers.keep]\ncommand = \"keep\"\nenv = {\n  TOKEN = \"preserved\",\n}\n";
        let codex_documents =
            document_set(AppType::Codex, &[(LogicalTarget::CodexConfig, live_mcp)]);
        let codex = standard_plan(
            AppType::Codex,
            NativeAction::Apply,
            "custom",
            json!({
                "auth": {},
                "config": "model = \"gpt-5\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://example.com\"\n"
            }),
            NativeProviderMode::Custom,
            &codex_documents,
            None,
        )
        .expect("Codex accepts TOML 1.1 MCP syntax");
        let codex_config = write_contents(&codex, LogicalTarget::CodexConfig);
        assert!(codex_config.contains("TOKEN = \"preserved\""));

        let grok_documents =
            document_set(AppType::GrokBuild, &[(LogicalTarget::GrokConfig, live_mcp)]);
        let grok = standard_plan(
            AppType::GrokBuild,
            NativeAction::Apply,
            "custom",
            json!({
                "config": "[models]\ndefault = \"grok-custom\"\n\n[model.grok-custom]\nmodel = \"grok-4.5\"\nbase_url = \"https://example.com/v1\"\nname = \"Example\"\napi_key = \"secret\"\napi_backend = \"responses\"\ncontext_window = 500000\n"
            }),
            NativeProviderMode::Custom,
            &grok_documents,
            None,
        )
        .expect("Grok accepts TOML 1.1 MCP syntax");
        assert!(write_contents(&grok, LogicalTarget::GrokConfig).contains("TOKEN = \"preserved\""));
    }

    #[test]
    fn gemini_and_grok_apply_preserve_host_owned_mcp_configuration() {
        let gemini_documents = document_set(
            AppType::Gemini,
            &[(
                LogicalTarget::GeminiSettings,
                r#"{"theme":"dark","mcpServers":{"keep":true}}"#,
            )],
        );
        let gemini = standard_plan(
            AppType::Gemini,
            NativeAction::Apply,
            "custom",
            json!({
                "env": {"GEMINI_API_KEY": "secret"},
                "config": {"theme": "light"}
            }),
            NativeProviderMode::Custom,
            &gemini_documents,
            Some("{\"CUSTOM_FLAG\":\"enabled\"}"),
        )
        .expect("valid Gemini plan");
        let settings: Value =
            serde_json::from_str(write_contents(&gemini, LogicalTarget::GeminiSettings))
                .expect("Gemini settings JSON");
        assert_eq!(settings["theme"], "light");
        assert_eq!(settings["mcpServers"]["keep"], true);
        assert_eq!(
            settings.pointer("/security/auth/selectedType"),
            Some(&json!("gemini-api-key"))
        );
        assert!(write_contents(&gemini, LogicalTarget::GeminiEnv).contains("CUSTOM_FLAG=enabled"));

        let grok_documents = document_set(
            AppType::GrokBuild,
            &[(
                LogicalTarget::GrokConfig,
                "[mcp_servers.keep]\ncommand = \"keep\"\n",
            )],
        );
        let grok = standard_plan(
            AppType::GrokBuild,
            NativeAction::Apply,
            "custom",
            json!({
                "config": "[models]\ndefault = \"grok-custom\"\n\n[model.grok-custom]\nmodel = \"grok-4.5\"\nbase_url = \"https://example.com/v1\"\nname = \"Example\"\napi_key = \"secret\"\napi_backend = \"responses\"\ncontext_window = 500000\n"
            }),
            NativeProviderMode::Custom,
            &grok_documents,
            None,
        )
        .expect("valid Grok plan");
        assert!(write_contents(&grok, LogicalTarget::GrokConfig).contains("[mcp_servers.keep]"));
    }

    #[test]
    fn additive_json_apps_apply_and_remove_without_losing_unrelated_data() {
        let opencode_documents = document_set(
            AppType::OpenCode,
            &[(
                LogicalTarget::OpenCodeConfig,
                r#"{"theme":"dark","provider":{"existing":{"options":{}}}}"#,
            )],
        );
        let opencode = standard_plan(
            AppType::OpenCode,
            NativeAction::Apply,
            "new",
            json!({"npm": "@ai-sdk/openai-compatible"}),
            NativeProviderMode::Custom,
            &opencode_documents,
            None,
        )
        .expect("valid OpenCode plan");
        let opencode_text = write_contents(&opencode, LogicalTarget::OpenCodeConfig);
        let opencode_value: Value = serde_json::from_str(opencode_text).expect("OpenCode JSON");
        assert_eq!(opencode_value["theme"], "dark");
        assert!(opencode_value["provider"]["existing"].is_object());
        assert_eq!(
            opencode_value["provider"]["new"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        let opencode_after_apply = document_set(
            AppType::OpenCode,
            &[(LogicalTarget::OpenCodeConfig, opencode_text)],
        );
        let opencode_removed = standard_plan(
            AppType::OpenCode,
            NativeAction::Remove,
            "new",
            json!({}),
            NativeProviderMode::Custom,
            &opencode_after_apply,
            None,
        )
        .expect("valid OpenCode removal");
        let removed: Value = serde_json::from_str(write_contents(
            &opencode_removed,
            LogicalTarget::OpenCodeConfig,
        ))
        .expect("OpenCode removal JSON");
        assert!(removed["provider"].get("new").is_none());
        assert!(removed["provider"]["existing"].is_object());

        let pi_documents = document_set(
            AppType::Pi,
            &[(
                LogicalTarget::PiModels,
                r#"{"future":true,"providers":{"existing":{"models":[]}}}"#,
            )],
        );
        let pi = standard_plan(
            AppType::Pi,
            NativeAction::Apply,
            "new",
            json!({"models": [{"id": "model"}]}),
            NativeProviderMode::Custom,
            &pi_documents,
            None,
        )
        .expect("valid Pi plan");
        let pi_text = write_contents(&pi, LogicalTarget::PiModels);
        let pi_value: Value = serde_json::from_str(pi_text).expect("Pi JSON");
        assert_eq!(pi_value["future"], true);
        assert_eq!(pi_value["providers"]["new"]["models"][0]["id"], "model");
        let pi_after_apply = document_set(AppType::Pi, &[(LogicalTarget::PiModels, pi_text)]);
        let pi_removed = standard_plan(
            AppType::Pi,
            NativeAction::Remove,
            "new",
            json!({}),
            NativeProviderMode::Custom,
            &pi_after_apply,
            None,
        )
        .expect("valid Pi removal");
        let removed: Value =
            serde_json::from_str(write_contents(&pi_removed, LogicalTarget::PiModels))
                .expect("Pi removal JSON");
        assert!(removed["providers"].get("new").is_none());
        assert!(removed["providers"]["existing"].is_object());
    }

    #[test]
    fn additive_json_semantics_accept_utf16_surrogate_pair_escapes() {
        let cases = [
            (
                AppType::OpenCode,
                LogicalTarget::OpenCodeConfig,
                r#"{provider:{existing:{npm:"package",label:"\uD83D\uDE00"}}}"#,
                json!({"npm": "new-package"}),
                "/provider/existing/label",
            ),
            (
                AppType::Pi,
                LogicalTarget::PiModels,
                r#"{providers:{existing:{label:"\uD83D\uDE00"}}}"#,
                json!({"models": []}),
                "/providers/existing/label",
            ),
        ];

        for (app, target, source, settings, pointer) in cases {
            let documents = document_set(app.clone(), &[(target, source)]);
            let plan = standard_plan(
                app,
                NativeAction::Apply,
                "new",
                settings,
                NativeProviderMode::Custom,
                &documents,
                None,
            )
            .expect("legacy JSON5 input remains accepted");
            let value: Value =
                serde_json::from_str(write_contents(&plan, target)).expect("projected JSON");
            assert_eq!(value.pointer(pointer), Some(&json!("😀")));
        }
    }

    #[test]
    fn openclaw_round_trip_keeps_comments_and_removes_only_the_selected_provider() {
        let documents = document_set(
            AppType::OpenClaw,
            &[(
                LogicalTarget::OpenClawConfig,
                "{\n  // keep this comment\n  tools: { profile: 'coding' },\n  models: { mode: 'merge', providers: { existing: { models: [] } } },\n}\n",
            )],
        );
        let applied = standard_plan(
            AppType::OpenClaw,
            NativeAction::Apply,
            "new",
            json!({"models": [{"id": "model"}]}),
            NativeProviderMode::Custom,
            &documents,
            None,
        )
        .expect("valid OpenClaw plan");
        let applied_text = write_contents(&applied, LogicalTarget::OpenClawConfig);
        assert!(applied_text.contains("// keep this comment"));
        assert!(applied_text.contains("tools: { profile: 'coding' }"));
        let value: Value = json_five::from_str(applied_text).expect("OpenClaw JSON5");
        assert_eq!(
            value["models"]["providers"]["new"]["models"][0]["id"],
            "model"
        );

        let after_apply = document_set(
            AppType::OpenClaw,
            &[(LogicalTarget::OpenClawConfig, applied_text)],
        );
        let removed = standard_plan(
            AppType::OpenClaw,
            NativeAction::Remove,
            "new",
            json!({}),
            NativeProviderMode::Custom,
            &after_apply,
            None,
        )
        .expect("valid OpenClaw removal");
        let removed_text = write_contents(&removed, LogicalTarget::OpenClawConfig);
        assert!(removed_text.contains("// keep this comment"));
        let value: Value = json_five::from_str(removed_text).expect("OpenClaw removal JSON5");
        assert!(value["models"]["providers"].get("new").is_none());
        assert!(value["models"]["providers"]["existing"].is_object());
    }

    #[test]
    fn claude_desktop_direct_mode_projects_profile_and_deployment_metadata() {
        let documents = document_set(
            AppType::ClaudeDesktop,
            &[
                (LogicalTarget::ClaudeDesktopNormalConfig, r#"{"keep":true}"#),
                (
                    LogicalTarget::ClaudeDesktopThreepConfig,
                    r#"{"enterpriseConfig":{"future":true}}"#,
                ),
                (
                    LogicalTarget::ClaudeDesktopMeta,
                    r#"{"entries":[{"id":"keep","name":"Keep"}],"appliedId":"keep"}"#,
                ),
            ],
        );
        let provider = ProviderSnapshot::new(
            "desktop-direct",
            AppType::ClaudeDesktop,
            "Desktop Direct",
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://example.com",
                    "ANTHROPIC_AUTH_TOKEN": "secret"
                }
            }),
        );
        let routes = [claude_desktop::DirectModelRoute {
            route_id: "claude-sonnet-4-6".to_owned(),
            upstream_model: "claude-sonnet-4-6".to_owned(),
            label_override: None,
            supports_1m: false,
        }];
        let plan = builtin_app_adapter(&AppType::ClaudeDesktop)
            .plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &provider,
                documents: &documents,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::ClaudeDesktop { routes: &routes },
            })
            .expect("valid Claude Desktop plan");

        assert_eq!(plan.writes.len(), 4);
        let normal: Value = serde_json::from_str(write_contents(
            &plan,
            LogicalTarget::ClaudeDesktopNormalConfig,
        ))
        .expect("normal Desktop config");
        assert_eq!(normal["deploymentMode"], "3p");
        assert_eq!(normal["keep"], true);
        let profile: Value =
            serde_json::from_str(write_contents(&plan, LogicalTarget::ClaudeDesktopProfile))
                .expect("Desktop profile");
        assert_eq!(profile["inferenceGatewayApiKey"], "secret");
        assert_eq!(profile["inferenceModels"][0], "claude-sonnet-4-6");
        let meta: Value =
            serde_json::from_str(write_contents(&plan, LogicalTarget::ClaudeDesktopMeta))
                .expect("Desktop metadata");
        assert_eq!(meta["appliedId"], CLAUDE_DESKTOP_PROFILE_ID);
        assert!(meta["entries"]
            .as_array()
            .is_some_and(|entries| entries.len() == 2));
    }

    #[test]
    fn hermes_apply_and_remove_preserve_unowned_yaml_fields() {
        let documents = document_set(
            AppType::Hermes,
            &[(
                LogicalTarget::HermesConfig,
                "model:\n  provider: old\n  default: old-model\n  context_length: 32000\ncustom_providers:\n  - name: old\n    base_url: https://old.example.com\n  - name: keep\n    base_url: https://keep.example.com\nfuture:\n  enabled: true\n",
            )],
        );
        let applied = standard_plan(
            AppType::Hermes,
            NativeAction::Apply,
            "new",
            json!({
                "_cc_source": "providers_dict",
                "base_url": "https://new.example.com",
                "models": [{"id": "new-model"}]
            }),
            NativeProviderMode::Custom,
            &documents,
            None,
        )
        .expect("valid Hermes plan");
        let applied_text = write_contents(&applied, LogicalTarget::HermesConfig);
        let applied_yaml: serde_yaml::Value =
            serde_yaml::from_str(applied_text).expect("Hermes YAML");
        assert_eq!(applied_yaml["model"]["provider"].as_str(), Some("new"));
        assert_eq!(
            applied_yaml["model"]["context_length"].as_i64(),
            Some(32000)
        );
        assert_eq!(applied_yaml["future"]["enabled"].as_bool(), Some(true));
        let projected = applied_yaml["custom_providers"]
            .as_sequence()
            .and_then(|providers| {
                providers.iter().find(|provider| {
                    provider.get("name").and_then(serde_yaml::Value::as_str) == Some("new")
                })
            })
            .expect("projected Hermes provider");
        assert!(projected.get("_cc_source").is_none());

        let after_apply = document_set(
            AppType::Hermes,
            &[(LogicalTarget::HermesConfig, applied_text)],
        );
        let removed = standard_plan(
            AppType::Hermes,
            NativeAction::Remove,
            "new",
            json!({}),
            NativeProviderMode::Custom,
            &after_apply,
            None,
        )
        .expect("valid Hermes removal");
        let removed_yaml: serde_yaml::Value =
            serde_yaml::from_str(write_contents(&removed, LogicalTarget::HermesConfig))
                .expect("Hermes removal YAML");
        assert!(removed_yaml["model"].get("provider").is_none());
        assert!(removed_yaml["model"].get("default").is_none());
        assert_eq!(removed_yaml["future"]["enabled"].as_bool(), Some(true));
        assert!(removed_yaml["custom_providers"]
            .as_sequence()
            .is_some_and(|providers| providers.iter().any(|provider| {
                provider.get("name").and_then(serde_yaml::Value::as_str) == Some("keep")
            })));
    }

    #[test]
    fn malformed_host_documents_fail_before_a_plan_is_returned() {
        let documents = document_set(AppType::Gemini, &[(LogicalTarget::GeminiSettings, "[]")]);
        assert!(matches!(
            standard_plan(
                AppType::Gemini,
                NativeAction::Apply,
                "custom",
                json!({"env": {"GEMINI_API_KEY": "secret"}}),
                NativeProviderMode::Custom,
                &documents,
                None,
            ),
            Err(NativePlanError::InvalidDocument {
                target: LogicalTarget::GeminiSettings,
                ..
            })
        ));

        let documents = document_set(
            AppType::Hermes,
            &[(
                LogicalTarget::HermesConfig,
                "custom_providers: []\ncustom_providers: []\n",
            )],
        );
        assert!(matches!(
            standard_plan(
                AppType::Hermes,
                NativeAction::Apply,
                "custom",
                json!({}),
                NativeProviderMode::Custom,
                &documents,
                None,
            ),
            Err(NativePlanError::InvalidDocument {
                target: LogicalTarget::HermesConfig,
                ..
            })
        ));

        let blank_json5 = document_set(
            AppType::OpenCode,
            &[(LogicalTarget::OpenCodeConfig, " \n\t")],
        );
        assert!(matches!(
            standard_plan(
                AppType::OpenCode,
                NativeAction::Apply,
                "custom",
                json!({"npm": "@ai-sdk/openai-compatible"}),
                NativeProviderMode::Custom,
                &blank_json5,
                None,
            ),
            Err(NativePlanError::InvalidDocument {
                target: LogicalTarget::OpenCodeConfig,
                ..
            })
        ));

        let blank_desktop = document_set(
            AppType::ClaudeDesktop,
            &[(LogicalTarget::ClaudeDesktopNormalConfig, "")],
        );
        let desktop = ProviderSnapshot::new(
            "desktop",
            AppType::ClaudeDesktop,
            "Desktop",
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://example.com",
                    "ANTHROPIC_AUTH_TOKEN": "secret"
                }
            }),
        );
        assert!(matches!(
            builtin_app_adapter(&AppType::ClaudeDesktop).plan_native(&NativePlanRequest {
                action: NativeAction::Apply,
                provider: &desktop,
                documents: &blank_desktop,
                mode: NativeProviderMode::Custom,
                access: NativeProviderAccess::Writable,
                context: NativePlanContext::ClaudeDesktop { routes: &[] },
            }),
            Err(NativePlanError::InvalidDocument {
                target: LogicalTarget::ClaudeDesktopNormalConfig,
                ..
            })
        ));
    }
}
