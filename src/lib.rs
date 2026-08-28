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
pub mod fs;
pub mod gemini;
pub mod grokbuild;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
mod operation;
pub mod pi;
mod provider;
mod registry;

pub use adapter::{builtin_app_adapter, builtin_app_adapters, AppAdapter};
pub use app_type::{AppType, ParseAppTypeError};
pub use operation::{
    ConfigFormat, ContentExpectation, LogicalTarget, OperationPlan, OperationPlanDecodeError,
    OperationPlanError, PlannedWrite, MAX_OPERATION_CONTENT_BYTES, MAX_OPERATION_PLAN_WIRE_BYTES,
    MAX_OPERATION_WRITES, OPERATION_CONTRACT_MAJOR,
};
pub use provider::{ProviderEntry, ProviderSnapshot};
pub use registry::{
    builtin_app_registry, AppCapability, AppDescriptor, BuiltinAppRegistry,
    ProviderConfigurationMode,
};
