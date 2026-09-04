//! Shared domain and live-configuration primitives for CC Switch applications.
//!
//! The crate contains application identifiers, safe file-writing primitives,
//! and small configuration adapters shared by CC Switch applications.
//!
//! ```
//! use cc_switch_core::AppType;
//!
//! let app = "codex".parse::<AppType>().expect("known app id");
//! assert_eq!(app.as_str(), "codex");
//! ```

mod adapter;
mod app_type;
pub mod claude;
pub mod claude_desktop;
pub mod codex;
pub mod common_config;
mod document;
mod executor;
pub mod fs;
pub mod gemini;
pub mod grokbuild;
pub mod hermes;
mod integration;
mod json5_patch;
mod mcp;
mod native_import;
pub mod openclaw;
pub mod opencode;
mod operation;
pub mod pi;
mod projection;
mod provider;
mod registry;
mod simple_provider;
mod skill;
mod yaml_patch;

pub use adapter::{builtin_app_adapter, builtin_app_adapters, AppAdapter};
pub use app_type::{AppType, ParseAppTypeError};
pub use document::{LiveDocumentSet, LiveDocumentSetError, ObservedDocument};
pub use executor::{
    execute_dependency_ordered_plan, execute_operation_plan, CompareExchangeOutcome,
    OperationExecutionError, OperationFailure, OperationHost, OperationRead, OperationReceipt,
    OperationRollbackError, OperationRollbackFailure,
};
pub use mcp::{
    import_mcp_servers, mcp_app_contract, mcp_config_target, mcp_servers_equivalent,
    project_mcp_server, project_mcp_servers, replace_mcp_servers, validate_mcp_server,
    validate_mcp_server_for_app, McpAppContract, McpConfigError, McpConfigTarget, McpImport,
    McpNativeSnapshot, McpServerProjection,
};
pub use native_import::{
    HermesProviderSource, NativeImportCandidate, NativeImportContext, NativeImportError,
    NativeImportStep,
};
pub use operation::{
    ConfigFormat, ContentExpectation, LogicalTarget, OperationPlan, OperationPlanDecodeError,
    OperationPlanError, PlannedWrite, MAX_OPERATION_CONTENT_BYTES, MAX_OPERATION_PLAN_WIRE_BYTES,
    MAX_OPERATION_WRITES, OPERATION_CONTRACT_MAJOR,
};
pub use projection::{
    CodexDocumentProjection, NativeAction, NativeDocumentProjection, NativePlanContext,
    NativePlanError, NativePlanPolicy, NativePlanRequest, NativePolicyPlanRequest,
    NativeProviderAccess, NativeProviderMode, MAX_NATIVE_PLAN_INPUT_BYTES, MAX_NATIVE_PLAN_ROUTES,
};
pub use provider::{ProviderEntry, ProviderSnapshot};
pub use registry::{
    builtin_app_registry, AppCapability, AppDescriptor, BuiltinAppRegistry,
    ProviderConfigurationMode,
};
pub use simple_provider::{
    builtin_simple_provider_forms, extract_simple_provider_values,
    project_simple_provider_settings, simple_provider_form, SimpleProviderError,
    SimpleProviderField, SimpleProviderFieldDescriptor, SimpleProviderFormDescriptor,
    SimpleProviderPreset, SimpleProviderProtocol, SimpleProviderValues,
};
pub use skill::{
    apply_skill_reference, execute_skill_live_plan, inspect_installed_skills,
    prepare_skill_reconciliation, prepare_skill_switch, skill_catalog_columns,
    InstalledSkillSnapshot, SkillAppContract, SkillAppRuntime, SkillAppState, SkillCatalogChange,
    SkillCatalogColumn, SkillCatalogDecision, SkillCatalogEntry, SkillCatalogEntryError,
    SkillCatalogGuard, SkillConfigTarget, SkillControlReason, SkillDiscovery,
    SkillLiveExecutionError, SkillLiveFailure, SkillLiveReceipt, SkillLiveRollbackError,
    SkillLiveRollbackFailure, SkillPrepareError, SkillReadError, SkillReferenceError,
    SkillReferencePlan, SkillReferenceReceipt, SkillRuntime, SkillRuntimeError, SkillSwitchPlan,
    SkillWriteOrder,
};
