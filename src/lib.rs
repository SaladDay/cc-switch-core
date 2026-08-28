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

mod app_type;
pub mod claude;
pub mod claude_desktop;
pub mod codex;
pub mod fs;
pub mod gemini;
pub mod grokbuild;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
pub mod pi;
mod provider;

pub use app_type::{AppType, ParseAppTypeError};
pub use provider::{ProviderEntry, ProviderSnapshot};
