use serde_json::{Map, Value};
use thiserror::Error;

/// A missing or invalid field in the minimum MCP connection contract.
///
/// Separate variants let hosts localize errors without parsing error messages.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum McpConnectionError {
    #[error("the definition must be an object")]
    NotObject,
    #[error("'type' must be a string")]
    NonStringType,
    #[error("unsupported transport '{0}'")]
    UnsupportedTransport(String),
    #[error("stdio definitions require command")]
    MissingCommand,
    #[error("remote definitions require url")]
    MissingHttpUrl,
    #[error("remote definitions require url")]
    MissingSseUrl,
}

/// Checks the transport and its required connection field without changing data.
///
/// An absent `type` means `stdio`. An explicit type must be `stdio`, `http`, or
/// `sse`. Stdio requires a string `command`; HTTP/SSE require a string `url`.
/// Required strings must contain a non-whitespace character. This checks neither
/// command availability nor URL syntax, authentication, or network access.
///
/// IDs, size limits, optional fields, native aliases and extensions are outside
/// this contract. Passing it does not imply that a server is canonical or can be
/// projected to every App. Use [`crate::validate_mcp_server`] or
/// [`crate::validate_mcp_server_for_app`] for those stricter checks.
///
/// ```
/// use cc_switch_core::{validate_mcp_connection, McpConnectionError};
/// use serde_json::json;
///
/// assert_eq!(validate_mcp_connection(&json!({"command":"node"})), Ok(()));
/// assert_eq!(validate_mcp_connection(&json!({"type":"http"})),
///     Err(McpConnectionError::MissingHttpUrl));
/// ```
pub fn validate_mcp_connection(server: &Value) -> Result<(), McpConnectionError> {
    let object = server.as_object().ok_or(McpConnectionError::NotObject)?;
    validate_fields(object).map(|_| ())
}

#[derive(Clone, Copy)]
pub(super) enum Transport {
    Stdio,
    Http,
    Sse,
}

impl Transport {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

pub(super) fn validate_fields(
    object: &Map<String, Value>,
) -> Result<Transport, McpConnectionError> {
    let transport = match object.get("type") {
        Some(Value::String(transport)) => transport.as_str(),
        Some(_) => return Err(McpConnectionError::NonStringType),
        None => "stdio",
    };
    let (transport, field, missing) = match transport {
        "stdio" => (
            Transport::Stdio,
            "command",
            McpConnectionError::MissingCommand,
        ),
        "http" => (Transport::Http, "url", McpConnectionError::MissingHttpUrl),
        "sse" => (Transport::Sse, "url", McpConnectionError::MissingSseUrl),
        other => return Err(McpConnectionError::UnsupportedTransport(other.to_owned())),
    };
    if object
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        Ok(transport)
    } else {
        Err(missing)
    }
}
