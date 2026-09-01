use std::collections::HashSet;

use serde_json::Value;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Table};

use crate::{SkillConfigTarget, MAX_OPERATION_CONTENT_BYTES};

use crate::{json5_patch, yaml_patch};

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
        disabled: HashSet<String>,
    },
    Grok {
        disabled: HashSet<String>,
    },
    Hermes {
        disabled: HashSet<String>,
        platform_disabled: HashSet<String>,
    },
}

impl NativeSkillControls {
    pub(super) fn control_for(&self, name: &str, directory: &str) -> NativeSkillControl {
        match self {
            Self::Gemini {
                globally_enabled,
                disabled,
            } => {
                if !globally_enabled {
                    NativeSkillControl::GloballyDisabled
                } else if disabled.contains(&name.to_lowercase()) {
                    NativeSkillControl::Disabled
                } else {
                    NativeSkillControl::Enabled
                }
            }
            Self::Grok { disabled } => {
                if disabled.contains(name) {
                    NativeSkillControl::Disabled
                } else {
                    NativeSkillControl::Enabled
                }
            }
            Self::Hermes {
                disabled,
                platform_disabled,
            } => {
                if directory == "hermes-agent" {
                    NativeSkillControl::Required
                } else if platform_disabled.contains(name) {
                    NativeSkillControl::ExternallyDisabled
                } else if disabled.contains(name) {
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
    platform: Option<&str>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    if contents.is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES) {
        return Err(invalid(target, "document is too large"));
    }
    match target {
        SkillConfigTarget::GeminiSettings => parse_gemini(target, contents),
        SkillConfigTarget::GrokConfig => parse_grok(target, contents),
        SkillConfigTarget::HermesConfig => parse_hermes(target, contents, platform),
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
            disabled: HashSet::new(),
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
            disabled: HashSet::new(),
        });
    };
    let skills = skills
        .as_table_like()
        .ok_or_else(|| invalid(target, "'skills' must be a table"))?;
    let Some(disabled) = skills.get("disabled") else {
        return Ok(NativeSkillControls::Grok {
            disabled: HashSet::new(),
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
        .collect::<Result<HashSet<_>, _>>()?;
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
    platform: Option<&str>,
) -> Result<NativeSkillControls, SkillConfigReadError> {
    let root = parse_yaml(target, contents)?;
    let skills_key = serde_yaml::Value::String("skills".to_owned());
    let Some(skills) = root.as_mapping().and_then(|root| root.get(&skills_key)) else {
        return Ok(NativeSkillControls::Hermes {
            disabled: HashSet::new(),
            platform_disabled: HashSet::new(),
        });
    };
    if skills.is_null() {
        return Ok(NativeSkillControls::Hermes {
            disabled: HashSet::new(),
            platform_disabled: HashSet::new(),
        });
    }
    let skills = skills
        .as_mapping()
        .ok_or_else(|| invalid(target, "'skills' must be a mapping"))?;

    let platform_disabled = match platform {
        None => HashSet::new(),
        Some(platform) => {
            let platform_key = serde_yaml::Value::String("platform_disabled".to_owned());
            match skills.get(&platform_key).filter(|value| !value.is_null()) {
                None => HashSet::new(),
                Some(platforms) => {
                    let platforms = platforms.as_mapping().ok_or_else(|| {
                        invalid(target, "'skills.platform_disabled' must be a mapping")
                    })?;
                    match platforms.get(serde_yaml::Value::String(platform.to_owned())) {
                        None | Some(serde_yaml::Value::Null) => HashSet::new(),
                        Some(value) => {
                            yaml_name_list(target, value, "'skills.platform_disabled.*'")?
                                .into_iter()
                                .collect()
                        }
                    }
                }
            }
        }
    };
    let disabled_key = serde_yaml::Value::String("disabled".to_owned());
    let disabled = match skills.get(&disabled_key) {
        None | Some(serde_yaml::Value::Null) => HashSet::new(),
        Some(value) => yaml_name_list(target, value, "'skills.disabled'")?
            .into_iter()
            .collect(),
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

pub(super) fn project_native_control(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    platform: Option<&str>,
    name: &str,
    directory: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigWriteError> {
    if name.is_empty()
        || name.trim() != name
        || name.len() > 256
        || name.chars().any(char::is_control)
    {
        return Err(write_invalid(target, "Skill name is invalid"));
    }
    if contents.is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES) {
        return Err(write_invalid(target, "document is too large"));
    }
    match target {
        SkillConfigTarget::GeminiSettings => project_gemini(target, contents, name, enabled),
        SkillConfigTarget::GrokConfig => project_grok(target, contents, name, enabled),
        SkillConfigTarget::HermesConfig => {
            project_hermes(target, contents, platform, name, directory, enabled)
        }
    }
}

fn project_gemini(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigWriteError> {
    let original = optional_utf8(target, contents)?;
    if let Some(original) = original.filter(|original| !original.is_empty()) {
        if json5_patch::object_path_has_comments(original, &["skills", "disabled"])
            .map_err(|message| write_invalid(target, &message))?
        {
            return Err(write_invalid(
                target,
                "'skills.disabled' contains comments that cannot be preserved safely",
            ));
        }
    }
    let mut root = parse_json(target, contents).map_err(SkillConfigWriteError::from_read)?;
    let skills = root
        .get("skills")
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| write_invalid(target, "'skills' must be an object"))
        })
        .transpose()?;
    let globally_enabled = skills
        .and_then(|skills| skills.get("enabled"))
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| write_invalid(target, "'skills.enabled' must be a boolean"))
        })
        .transpose()?
        .unwrap_or(true);
    if enabled && !globally_enabled {
        return Err(SkillConfigWriteError::GloballyDisabled { target });
    }
    let disabled = skills
        .map(|skills| {
            json_name_list(target, skills.get("disabled"), "'skills.disabled'")
                .map_err(SkillConfigWriteError::from_read)
        })
        .transpose()?
        .unwrap_or_default();
    let contains = disabled
        .iter()
        .any(|entry| entry.to_lowercase() == name.to_lowercase());
    if contains != enabled {
        return Ok(None);
    }

    let next = changed_names(disabled, name, enabled, |left, right| {
        left.to_lowercase() == right.to_lowercase()
    });
    let root_object = root
        .as_object_mut()
        .expect("validated Gemini settings have an object root");
    let skills = root_object
        .entry("skills")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .expect("validated Gemini skills are an object");
    skills.insert(
        "disabled".to_owned(),
        Value::Array(next.into_iter().map(Value::String).collect()),
    );
    let projected = match original.filter(|original| !original.is_empty()) {
        Some(original) => json5_patch::replace_object_path_value(
            original,
            &["skills", "disabled"],
            &root["skills"]["disabled"],
        )
        .map_err(|message| write_invalid(target, &message))?,
        None => pretty_json(&root).map_err(|message| write_invalid(target, &message))?,
    };
    bounded_projection(target, projected)
}

fn project_grok(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigWriteError> {
    let mut document = parse_toml(target, contents).map_err(SkillConfigWriteError::from_read)?;
    let disabled = toml_disabled_names(target, &document)?;
    let contains = disabled.iter().any(|entry| entry == name);
    if contains != enabled {
        return Ok(None);
    }
    if document.get("skills").is_none() {
        document["skills"] = Item::Table(Table::new());
    }
    let skills = document["skills"]
        .as_table_like_mut()
        .expect("validated Grok skills are a table");
    if skills.get("disabled").is_none() {
        skills.insert("disabled", Item::Value(Array::new().into()));
    }
    let disabled = skills
        .get_mut("disabled")
        .and_then(Item::as_array_mut)
        .expect("validated Grok disabled value is an array");
    if enabled {
        for index in (0..disabled.len()).rev() {
            if disabled.get(index).and_then(toml_edit::Value::as_str) == Some(name) {
                disabled.remove(index);
            }
        }
    } else {
        disabled.push(name);
    }
    bounded_projection(target, document.to_string())
}

fn toml_disabled_names(
    target: SkillConfigTarget,
    document: &DocumentMut,
) -> Result<Vec<String>, SkillConfigWriteError> {
    let Some(skills) = document.get("skills") else {
        return Ok(Vec::new());
    };
    let skills = skills
        .as_table_like()
        .ok_or_else(|| write_invalid(target, "'skills' must be a table"))?;
    let Some(disabled) = skills.get("disabled") else {
        return Ok(Vec::new());
    };
    let disabled = disabled
        .as_array()
        .ok_or_else(|| write_invalid(target, "'skills.disabled' must be an array"))?;
    if toml_raw_has_comment(disabled.trailing())
        || disabled.iter().any(|value| {
            [value.decor().prefix(), value.decor().suffix()]
                .into_iter()
                .flatten()
                .any(toml_raw_has_comment)
        })
    {
        return Err(write_invalid(
            target,
            "'skills.disabled' contains comments that cannot be preserved safely",
        ));
    }
    disabled
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| write_invalid(target, "'skills.disabled' must contain strings"))
        })
        .collect()
}

fn toml_raw_has_comment(raw: &toml_edit::RawString) -> bool {
    raw.as_str().is_none_or(|raw| raw.contains('#'))
}

fn project_hermes(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
    platform: Option<&str>,
    name: &str,
    directory: &str,
    enabled: bool,
) -> Result<Option<String>, SkillConfigWriteError> {
    if directory == "hermes-agent" && !enabled {
        return Err(SkillConfigWriteError::Required {
            target,
            directory: directory.to_owned(),
        });
    }
    let original = optional_utf8(target, contents)?.unwrap_or_default();
    if yaml_patch::has_duplicate_top_level_key(original, "skills") {
        return Err(write_invalid(target, "contains duplicate 'skills' fields"));
    }
    let mut root = parse_yaml(target, contents).map_err(SkillConfigWriteError::from_read)?;
    let controls =
        parse_hermes(target, contents, platform).map_err(SkillConfigWriteError::from_read)?;
    if enabled && controls.control_for(name, directory) == NativeSkillControl::ExternallyDisabled {
        return Err(SkillConfigWriteError::ExternallyDisabled { target });
    }
    let skills_key = serde_yaml::Value::String("skills".to_owned());
    let disabled_key = serde_yaml::Value::String("disabled".to_owned());
    let section_existed = root
        .as_mapping()
        .expect("validated Hermes config has a mapping root")
        .contains_key(&skills_key);
    let disabled = root
        .as_mapping()
        .and_then(|root| root.get(&skills_key))
        .filter(|skills| !skills.is_null())
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|skills| skills.get(&disabled_key))
        .filter(|value| !value.is_null())
        .map(|value| {
            yaml_name_list(target, value, "'skills.disabled'")
                .map_err(SkillConfigWriteError::from_read)
        })
        .transpose()?
        .unwrap_or_default();
    let contains = disabled.iter().any(|entry| entry == name);
    if contains != enabled {
        return Ok(None);
    }
    if yaml_patch::top_level_section_has_comments(original, "skills") {
        return Err(write_invalid(
            target,
            "the 'skills' section contains comments that cannot be preserved safely",
        ));
    }
    if yaml_patch::top_level_section_has_references(original, "skills") {
        return Err(write_invalid(
            target,
            "the 'skills' section contains YAML anchors, aliases, or merge keys that cannot be preserved safely",
        ));
    }

    let next = changed_names(disabled, name, enabled, |left, right| left == right);
    let root = root
        .as_mapping_mut()
        .expect("validated Hermes config has a mapping root");
    if !root
        .get(&skills_key)
        .is_some_and(serde_yaml::Value::is_mapping)
    {
        root.insert(
            skills_key.clone(),
            serde_yaml::Value::Mapping(Default::default()),
        );
    }
    let skills = root
        .get_mut(&skills_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .expect("projected Hermes skills are a mapping");
    skills.insert(
        disabled_key,
        serde_yaml::Value::Sequence(next.into_iter().map(serde_yaml::Value::String).collect()),
    );
    let projected = yaml_patch::replace_top_level_section(
        original,
        "skills",
        root.get(&skills_key)
            .expect("projected Hermes config contains skills"),
        section_existed,
    )
    .map_err(|message| write_invalid(target, &message))?;
    serde_yaml::from_str::<serde_yaml::Value>(&projected)
        .map_err(|_| write_invalid(target, "projected document is not valid YAML"))?;
    bounded_projection(target, projected)
}

fn changed_names<F>(names: Vec<String>, name: &str, enabled: bool, equivalent: F) -> Vec<String>
where
    F: Fn(&str, &str) -> bool,
{
    let mut next = names
        .into_iter()
        .filter(|entry| !enabled || !equivalent(entry, name))
        .collect::<Vec<_>>();
    if !enabled {
        next.push(name.to_owned());
    }
    next
}

fn optional_utf8(
    target: SkillConfigTarget,
    contents: Option<&[u8]>,
) -> Result<Option<&str>, SkillConfigWriteError> {
    contents
        .map(|contents| {
            std::str::from_utf8(contents)
                .map_err(|_| write_invalid(target, "document is not UTF-8"))
        })
        .transpose()
}

fn pretty_json(root: &Value) -> Result<String, String> {
    let mut rendered = serde_json::to_string_pretty(root).map_err(|error| error.to_string())?;
    rendered.push('\n');
    Ok(rendered)
}

fn bounded_projection(
    target: SkillConfigTarget,
    output: String,
) -> Result<Option<String>, SkillConfigWriteError> {
    if output.len() > MAX_OPERATION_CONTENT_BYTES {
        Err(write_invalid(target, "projected document is too large"))
    } else {
        Ok(Some(output))
    }
}

fn write_invalid(target: SkillConfigTarget, message: &str) -> SkillConfigWriteError {
    SkillConfigWriteError::Invalid {
        target,
        message: message.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(super) enum SkillConfigWriteError {
    #[error("invalid {target:?} Skill configuration: {message}")]
    Invalid {
        target: SkillConfigTarget,
        message: String,
    },
    #[error("{target:?} has disabled Skills globally")]
    GloballyDisabled { target: SkillConfigTarget },
    #[error("{target:?} has disabled this Skill for the active platform")]
    ExternallyDisabled { target: SkillConfigTarget },
    #[error("{target:?} requires Skill directory {directory:?}")]
    Required {
        target: SkillConfigTarget,
        directory: String,
    },
}

impl SkillConfigWriteError {
    fn from_read(error: SkillConfigReadError) -> Self {
        match error {
            SkillConfigReadError::Invalid { target, message } => Self::Invalid { target, message },
        }
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
        parse_native_controls(target, contents, None)
            .map(|controls| controls.control_for(name, name))
    }

    fn inspect_for_platform(
        target: SkillConfigTarget,
        contents: Option<&[u8]>,
        name: &str,
        platform: &str,
    ) -> Result<NativeSkillControl, SkillConfigReadError> {
        parse_native_controls(target, contents, Some(platform))
            .map(|controls| controls.control_for(name, name))
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
        let platform_controls =
            b"skills:\n  platform_disabled:\n    telegram: [demo, hermes-agent]\n";
        assert_eq!(
            inspect_for_platform(
                SkillConfigTarget::HermesConfig,
                Some(platform_controls),
                "demo",
                "telegram",
            ),
            Ok(NativeSkillControl::ExternallyDisabled)
        );
        assert_eq!(
            inspect_for_platform(
                SkillConfigTarget::HermesConfig,
                Some(platform_controls),
                "demo",
                "cli",
            ),
            Ok(NativeSkillControl::Enabled)
        );
        assert_eq!(
            inspect(
                SkillConfigTarget::HermesConfig,
                Some(platform_controls),
                "demo",
            ),
            Ok(NativeSkillControl::Enabled)
        );
        assert_eq!(
            inspect_for_platform(
                SkillConfigTarget::HermesConfig,
                Some(platform_controls),
                "hermes-agent",
                "telegram",
            ),
            Ok(NativeSkillControl::Required)
        );
        let controls = parse_native_controls(
            SkillConfigTarget::HermesConfig,
            Some(platform_controls),
            Some("telegram"),
        )
        .expect("Hermes controls");
        assert_eq!(
            controls.control_for("Hermes Agent", "hermes-agent"),
            NativeSkillControl::Required
        );
    }

    #[test]
    fn hermes_ignores_platform_controls_without_an_active_platform() {
        assert_eq!(
            inspect(
                SkillConfigTarget::HermesConfig,
                Some(b"skills:\n  platform_disabled: []\n"),
                "demo",
            ),
            Ok(NativeSkillControl::Enabled)
        );
    }

    #[test]
    fn malformed_native_controls_are_not_guessed() {
        assert!(inspect(SkillConfigTarget::GeminiSettings, Some(b"[]"), "demo").is_err());
        assert!(inspect(SkillConfigTarget::GrokConfig, Some(b"skills = 1"), "demo").is_err());
        assert!(inspect(SkillConfigTarget::HermesConfig, Some(b"skills: []"), "demo").is_err());
    }

    #[test]
    fn gemini_projection_changes_only_the_disabled_value() {
        let input = b"{\n  // keep\n  theme: 'dark',\n  skills: { disabled: ['Old'] },\n}\n";
        let output = project_native_control(
            SkillConfigTarget::GeminiSettings,
            Some(input),
            None,
            "old",
            "old",
            true,
        )
        .unwrap()
        .unwrap();

        assert!(output.contains("// keep"));
        assert!(output.contains("theme: 'dark'"));
        assert_eq!(
            inspect(
                SkillConfigTarget::GeminiSettings,
                Some(output.as_bytes()),
                "old"
            ),
            Ok(NativeSkillControl::Enabled)
        );
    }

    #[test]
    fn projection_is_idempotent_and_preserves_unrelated_toml() {
        let input = b"theme = 'dark'\n[skills]\ndisabled = ['old']\n";
        assert_eq!(
            project_native_control(
                SkillConfigTarget::GrokConfig,
                Some(input),
                None,
                "old",
                "old",
                false,
            )
            .unwrap(),
            None
        );
        let output = project_native_control(
            SkillConfigTarget::GrokConfig,
            Some(input),
            None,
            "old",
            "old",
            true,
        )
        .unwrap()
        .unwrap();
        assert!(output.contains("theme = 'dark'"));
        assert!(!output.contains("'old'"));
    }

    #[test]
    fn hermes_projection_preserves_other_top_level_sections() {
        let input = b"model:\n  default: test\nskills:\n  disabled: [old]\n  paths: [team]\n";
        let output = project_native_control(
            SkillConfigTarget::HermesConfig,
            Some(input),
            Some("cli"),
            "old",
            "old",
            true,
        )
        .unwrap()
        .unwrap();

        assert!(output.starts_with("model:\n  default: test\n"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["paths"][0], "team");
    }

    #[test]
    fn unsafe_or_constrained_native_edits_fail_closed() {
        assert!(project_native_control(
            SkillConfigTarget::GeminiSettings,
            Some(b"{ skills: { disabled: [/* keep */ 'old'] } }"),
            None,
            "old",
            "old",
            true,
        )
        .is_err());
        assert!(project_native_control(
            SkillConfigTarget::HermesConfig,
            Some(b"skills:\n  # keep\n  disabled: [old]\n"),
            None,
            "old",
            "old",
            true,
        )
        .is_err());
        assert!(matches!(
            project_native_control(
                SkillConfigTarget::HermesConfig,
                None,
                None,
                "Hermes Agent",
                "hermes-agent",
                false,
            ),
            Err(SkillConfigWriteError::Required { .. })
        ));
    }
}
