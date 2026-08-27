//! Shared domain primitives for CC Switch applications.
//!
//! The crate contains application identifiers and safe file-writing primitives
//! shared by CC Switch applications.
//!
//! ```
//! use cc_switch_core::AppType;
//!
//! let app = "codex".parse::<AppType>().expect("known app id");
//! assert_eq!(app.as_str(), "codex");
//! ```

mod app_type;
pub mod claude;
pub mod fs;

pub use app_type::{AppType, ParseAppTypeError};
