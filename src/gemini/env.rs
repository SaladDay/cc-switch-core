use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;

/// Render literal assignments in key order, separated by LF with no final LF.
/// Duplicate names retain their input order. Nothing is trimmed, quoted or escaped.
/// This formatter does not validate names, credentials or embedded CR/LF/NUL;
/// callers must apply their write policy before passing untrusted input.
///
/// ```
/// use cc_switch_core::gemini::render_literal_env_assignments;
/// assert_eq!(
///     render_literal_env_assignments([("Z", "'literal'"), ("A", "x=y")]),
///     "A=x=y\nZ='literal'",
/// );
/// ```
pub fn render_literal_env_assignments<'a>(
    assignments: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut assignments: Vec<_> = assignments.into_iter().collect();
    assignments.sort_by(|left, right| left.0.cmp(right.0));
    let mut output = String::new();
    for (index, (key, value)) in assignments.into_iter().enumerate() {
        if index != 0 {
            output.push('\n');
        }
        output.push_str(key);
        output.push('=');
        output.push_str(value);
    }
    output
}

/// Select literal string entries from an optional native `env` object.
/// Missing/non-object values yield no entries; non-string entries are omitted.
/// This field selector does not validate credentials, names or env-file safety.
pub fn select_string_env_values(env: Option<&Value>) -> BTreeMap<String, String> {
    env.and_then(Value::as_object)
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
        .collect()
}

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
