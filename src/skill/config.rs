use serde_json::Value;
use thiserror::Error;
use toml_edit::DocumentMut;

use crate::{SkillConfigTarget, MAX_OPERATION_CONTENT_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeSkillControl {
    Enabled,
    Disabled,
    Required,
    GloballyDisabled,
    ExternallyDisabled,
}

pub(super) enum NativeSkillControls {
    Gemini {
        globally_enabled: bool,
        disabled: Vec<String>,
    },
    Grok {
        disabled: Vec<String>,
    },
    Hermes {
        disabled: Vec<String>,
        platform_disabled: Vec<String>,
    },
}

impl NativeSkillControls {
    pub(super) fn control_for(&self, name: &str) -> NativeSkillControl {
        match self {
            Self::Gemini {
                globally_enabled,
                disabled,
            } => {
                if !globally_enabled {
                    NativeSkillControl::GloballyDisabled
                } else if disabled
                    .iter()
                    .any(|candidate| candidate == &name.to_lowercase())
                {
                    NativeSkillControl::Disabled
                } else {
                    NativeSkillControl::Enabled
                }
            }
            Self::Grok { disabled } => {
                if disabled.iter().any(|candidate| candidate == name) {
                    NativeSkillControl::Disabled
                } else {
                    NativeSkillControl::Enabled
                }
            }
            Self::Hermes {
                disabled,
                platform_disabled,
            } => {
                if platform_disabled.iter().any(|candidate| candidate == name) {
                    NativeSkillControl::ExternallyDisabled
                } else if name == "hermes-agent" {
                    NativeSkillControl::Required
                } else if disabled.iter().any(|candidate| candidate == name) {
                    NativeSkillControl::Disabled
                } else {
                    NativeSkillControl::Enabled
                }
            }
        }
    }
}

pub(super) fn parse_native_controls(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    if contents.is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES) {
        return Err(invalid(target, "document is too large"));
    }
    match target {
        SkillConfigTarget::GeminiSettings => parse_gemini(target, contents),
        SkillConfigTarget::GrokConfig => parse_grok(target, contents),
        SkillConfigTarget::HermesConfig => parse_hermes(target, contents),
    }
}

fn parse_gemini(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    let root = parse_json(target, contents)?;
    let Some(skills) = root.get("skills") else {
        return Ok(NativeSkillControls::Gemini {
            globally_enabled: true,
            disabled: Vec::new(),
        });
    };
    let skills = skills
        .as_object()
        .ok_or_else(|| invalid(target, "'skills' must be an object"))?;
    let globally_enabled = match skills.get("enabled") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid(target, "'skills.enabled' must be a boolean"))?,
    };
    let disabled = json_name_list(target, skills.get("disabled"), "'skills.disabled'")?
        .into_iter()
        .map(|name| name.to_lowercase())
        .collect();
    Ok(NativeSkillControls::Gemini {
        globally_enabled,
        disabled,
    })
}

fn parse_json(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<Value, SkillConfigReadError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(Value::Object(Default::default()));
    };
    let text =
        std::str::from_utf8(contents).map_err(|_| invalid(target, "document is not UTF-8"))?;
    let root = json5::from_str::<Value>(text)
        .map_err(|_| invalid(target, "document is not valid JSON settings"))?;
    if !root.is_object() {
        return Err(invalid(target, "root must be an object"));
    }
    Ok(root)
}

fn json_name_list(
    target: SkillConfigTarget,
    value: Option<&Value>,
    label: &str,
) -> Result<Vec<String>, SkillConfigReadError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| invalid(target, &format!("{label} must be an array")))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(target, &format!("{label} must contain strings")))
        })
        .collect()
}

fn parse_grok(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    let document = parse_toml(target, contents)?;
    let Some(skills) = document.get("skills") else {
        return Ok(NativeSkillControls::Grok {
            disabled: Vec::new(),
        });
    };
    let skills = skills
        .as_table_like()
        .ok_or_else(|| invalid(target, "'skills' must be a table"))?;
    let Some(disabled) = skills.get("disabled") else {
        return Ok(NativeSkillControls::Grok {
            disabled: Vec::new(),
        });
    };
    let disabled = disabled
        .as_array()
        .ok_or_else(|| invalid(target, "'skills.disabled' must be an array"))?;
    let disabled = disabled
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(target, "'skills.disabled' must contain strings"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NativeSkillControls::Grok { disabled })
}

fn parse_toml(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<DocumentMut, SkillConfigReadError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(DocumentMut::new());
    };
    let text =
        std::str::from_utf8(contents).map_err(|_| invalid(target, "document is not UTF-8"))?;
    text.parse::<DocumentMut>()
        .map_err(|_| invalid(target, "document is not valid TOML"))
}

fn parse_hermes(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    let root = parse_yaml(target, contents)?;
    let skills_key = serde_yaml::Value::String("skills".to_owned());
    let Some(skills) = root.as_mapping().and_then(|root| root.get(&skills_key)) else {
        return Ok(NativeSkillControls::Hermes {
            disabled: Vec::new(),
            platform_disabled: Vec::new(),
        });
    };
    if skills.is_null() {
        return Ok(NativeSkillControls::Hermes {
            disabled: Vec::new(),
            platform_disabled: Vec::new(),
        });
    }
    let skills = skills
        .as_mapping()
        .ok_or_else(|| invalid(target, "'skills' must be a mapping"))?;

    let platform_key = serde_yaml::Value::String("platform_disabled".to_owned());
    let mut platform_disabled = Vec::new();
    if let Some(platforms) = skills.get(&platform_key).filter(|value| !value.is_null()) {
        let platforms = platforms
            .as_mapping()
            .ok_or_else(|| invalid(target, "'skills.platform_disabled' must be a mapping"))?;
        for value in platforms.values() {
            platform_disabled.extend(yaml_name_list(
                target,
                value,
                "'skills.platform_disabled.*'",
            )?);
        }
    }

    let disabled_key = serde_yaml::Value::String("disabled".to_owned());
    let disabled = match skills.get(&disabled_key) {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(value) => yaml_name_list(target, value, "'skills.disabled'")?,
    };
    Ok(NativeSkillControls::Hermes {
        disabled,
        platform_disabled,
    })
}

fn parse_yaml(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<serde_yaml::Value, SkillConfigReadError> {
    let Some(contents) = contents.filter(|contents| !contents.is_empty()) else {
        return Ok(serde_yaml::Value::Mapping(Default::default()));
    };
    let text =
        std::str::from_utf8(contents).map_err(|_| invalid(target, "document is not UTF-8"))?;
    let root = serde_yaml::from_str::<serde_yaml::Value>(text)
        .map_err(|_| invalid(target, "document is not valid YAML"))?;
    if !root.is_mapping() {
        return Err(invalid(target, "root must be a mapping"));
    }
    Ok(root)
}

fn yaml_name_list(
    target: SkillConfigTarget,
    value: &serde_yaml::Value,
    label: &str,
) -> Result<Vec<String>, SkillConfigReadError> {
    match value {
        serde_yaml::Value::Null => Ok(Vec::new()),
        serde_yaml::Value::String(value) => {
            let value = value.trim();
            Ok((!value.is_empty())
                .then(|| value.to_owned())
                .into_iter()
                .collect())
        }
        serde_yaml::Value::Sequence(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| invalid(target, &format!("{label} must contain strings")))
            })
            .collect(),
        _ => Err(invalid(
            target,
            &format!("{label} must be a string or string array"),
        )),
    }
}

fn invalid(target: SkillConfigTarget, message: &str) -> SkillConfigReadError {
    SkillConfigReadError::Invalid {
        target,
        message: message.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(super) enum SkillConfigReadError {
    #[error("invalid {target:?} Skill configuration: {message}")]
    Invalid {
        target: SkillConfigTarget,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(
        target: SkillConfigTarget,
        contents: Option<&[u8]>,
        name: &str,
    ) -> Result<NativeSkillControl, SkillConfigReadError> {
        parse_native_controls(target, contents).map(|controls| controls.control_for(name))
    }

    #[test]
    fn gemini_reads_global_and_case_insensitive_disabled_state() {
        assert_eq!(
            inspect(
                SkillConfigTarget::GeminiSettings,
                Some(br#"{ skills: { disabled: ["DEMO"] } }"#),
                "demo",
            ),
            Ok(NativeSkillControl::Disabled)
        );
        assert_eq!(
            inspect(
                SkillConfigTarget::GeminiSettings,
                Some(br#"{ "skills": { "enabled": false } }"#),
                "demo",
            ),
            Ok(NativeSkillControl::GloballyDisabled)
        );
    }

    #[test]
    fn grok_reads_its_disabled_array() {
        assert_eq!(
            inspect(
                SkillConfigTarget::GrokConfig,
                Some(b"[skills]\ndisabled = [\"demo\"]\n"),
                "demo",
            ),
            Ok(NativeSkillControl::Disabled)
        );
    }

    #[test]
    fn hermes_preserves_required_and_platform_constraints() {
        assert_eq!(
            inspect(SkillConfigTarget::HermesConfig, None, "hermes-agent"),
            Ok(NativeSkillControl::Required)
        );
        assert_eq!(
            inspect(
                SkillConfigTarget::HermesConfig,
                Some(b"skills:\n  platform_disabled:\n    windows: demo\n"),
                "demo",
            ),
            Ok(NativeSkillControl::ExternallyDisabled)
        );
    }

    #[test]
    fn malformed_native_controls_are_not_guessed() {
        assert!(inspect(SkillConfigTarget::GeminiSettings, Some(b"[]"), "demo").is_err());
        assert!(inspect(SkillConfigTarget::GrokConfig, Some(b"skills = 1"), "demo").is_err());
        assert!(inspect(SkillConfigTarget::HermesConfig, Some(b"skills: []"), "demo").is_err());
    }
}
