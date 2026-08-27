//! Shared domain primitives for CC Switch applications.
//!
//! The crate starts with the application identifiers used by CC Switch. More
//! behavior will move here only when both desktop applications need it.
//!
//! ```
//! use cc_switch_core::AppType;
//!
//! let app = "codex".parse::<AppType>().expect("known app id");
//! assert_eq!(app.as_str(), "codex");
//! ```

mod app_type;

pub use app_type::{AppType, ParseAppTypeError};
