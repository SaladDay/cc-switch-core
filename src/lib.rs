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
pub mod codex;
pub mod fs;

pub use app_type::{AppType, ParseAppTypeError};
