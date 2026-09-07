use thiserror::Error;

/// Grammar for assignment lines, without shell expansion or quote removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvAssignmentSyntax {
    /// ASCII letters, digits and underscores in names; no CR, LF or NUL in values.
    Portable,
    /// Unicode alphanumeric names and underscores; values are literal text.
    /// Reading this grammar does not make its contents safe to write as an env file.
    UnicodeLiteral,
}

/// A redacted assignment error. Line numbers are one-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("env line {line}: {kind}")]
pub struct EnvAssignmentError {
    pub line: usize,
    pub kind: EnvAssignmentErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnvAssignmentErrorKind {
    #[error("missing '=' separator")]
    MissingSeparator,
    #[error("empty variable name")]
    EmptyKey,
    #[error("invalid variable name")]
    InvalidKey,
    #[error("invalid value")]
    InvalidValue,
}

/// Read assignment lines in source order, retaining duplicates and literal quotes.
/// Blank lines and comments are omitted; names and values are trimmed.
/// Collect the results to reject invalid lines, or discard errors for a tolerant
/// snapshot. Callers choose the duplicate-key policy when collecting assignments.
pub fn parse_env_assignments(
    content: &str,
    syntax: EnvAssignmentSyntax,
) -> impl Iterator<Item = Result<(&str, &str), EnvAssignmentError>> {
    content
        .lines()
        .enumerate()
        .filter_map(move |(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let parse = || {
                let (key, value) = line
                    .split_once('=')
                    .ok_or(EnvAssignmentErrorKind::MissingSeparator)?;
                let key = key.trim();
                if key.is_empty() {
                    return Err(EnvAssignmentErrorKind::EmptyKey);
                }
                if !key.chars().all(|character| {
                    character == '_'
                        || match syntax {
                            EnvAssignmentSyntax::Portable => character.is_ascii_alphanumeric(),
                            EnvAssignmentSyntax::UnicodeLiteral => character.is_alphanumeric(),
                        }
                }) {
                    return Err(EnvAssignmentErrorKind::InvalidKey);
                }
                let value = value.trim();
                if syntax == EnvAssignmentSyntax::Portable && value.contains(['\r', '\n', '\0']) {
                    return Err(EnvAssignmentErrorKind::InvalidValue);
                }
                Ok((key, value))
            };
            Some(parse().map_err(|kind| EnvAssignmentError {
                line: index + 1,
                kind,
            }))
        })
}
