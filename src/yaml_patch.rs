//! Minimal top-level YAML section updates that preserve unrelated source text.

pub(crate) fn replace_top_level_section(
    raw: &str,
    key: &str,
    value: &serde_yaml::Value,
    section_existed: bool,
) -> Result<String, String> {
    let mut section = serde_yaml::Mapping::new();
    section.insert(serde_yaml::Value::String(key.to_owned()), value.clone());
    let serialized = serde_yaml::to_string(&serde_yaml::Value::Mapping(section))
        .map_err(|error| format!("YAML section could not be serialized: {error}"))?;
    if let Some((start, end)) = section_range(raw, key) {
        let mut output = String::with_capacity(raw.len() + serialized.len());
        output.push_str(&raw[..start]);
        output.push_str(&serialized);
        output.push_str(&raw[end..]);
        Ok(output)
    } else if section_existed || uses_flow_root(raw) {
        Err(format!(
            "existing '{key}' section cannot be updated without rewriting unrelated YAML"
        ))
    } else {
        let mut output = raw.to_owned();
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&serialized);
        Ok(output)
    }
}

pub(crate) fn has_duplicate_top_level_key(raw: &str, key: &str) -> bool {
    let target = format!("{key}:");
    raw.lines()
        .filter(|line| top_level_key(line) && line.starts_with(&target))
        .take(2)
        .count()
        > 1
}

pub(crate) fn top_level_section_has_comments(raw: &str, key: &str) -> bool {
    section_range(raw, key)
        .map(|(start, end)| raw[start..end].lines().any(line_has_comment))
        .unwrap_or(false)
}

pub(crate) fn top_level_section_has_references(raw: &str, key: &str) -> bool {
    section_range(raw, key)
        .map(|(start, end)| raw[start..end].lines().any(line_has_reference))
        .unwrap_or(false)
}

fn line_has_reference(line: &str) -> bool {
    let code = unquoted_yaml_code(line);
    if code.trim_start().starts_with("<<:") {
        return true;
    }
    let characters = code.chars().collect::<Vec<_>>();
    characters.iter().enumerate().any(|(index, character)| {
        if !matches!(character, '&' | '*') {
            return false;
        }
        let starts_token = index == 0
            || characters[index - 1].is_whitespace()
            || matches!(characters[index - 1], ':' | ',' | '[' | '{' | '-' | '?');
        let has_name = characters
            .get(index + 1)
            .is_some_and(|next| !next.is_whitespace() && !matches!(next, ']' | '}' | ','));
        starts_token && has_name
    })
}

fn unquoted_yaml_code(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            output.push(' ');
            if escaped {
                escaped = false;
            } else if active_quote == '"' && character == '\\' {
                escaped = true;
            } else if character == active_quote {
                if active_quote == '\'' && characters.peek() == Some(&'\'') {
                    output.push(' ');
                    characters.next();
                } else {
                    quote = None;
                }
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
            output.push(' ');
        } else if character == '#' {
            break;
        } else {
            output.push(character);
        }
    }
    output
}

fn line_has_comment(line: &str) -> bool {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if double_quoted && character == '\\' {
            escaped = true;
        } else if !double_quoted && character == '\'' {
            single_quoted = !single_quoted;
        } else if !single_quoted && character == '"' {
            double_quoted = !double_quoted;
        } else if !single_quoted && !double_quoted && character == '#' {
            return true;
        }
    }
    false
}

fn uses_flow_root(raw: &str) -> bool {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('%'))
        .find_map(|line| {
            let line = line.strip_prefix("---").unwrap_or(line).trim_start();
            (!line.is_empty()).then_some(line.starts_with('{'))
        })
        .unwrap_or(false)
}

fn section_range(raw: &str, key: &str) -> Option<(usize, usize)> {
    let target = format!("{key}:");
    let mut start = None;
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        let plain = line.trim_end_matches(['\r', '\n']);
        if start.is_none() && top_level_key(plain) && plain.starts_with(&target) {
            start = Some(offset);
        } else if start.is_some() && top_level_key(plain) {
            return Some((start.expect("section start"), offset));
        }
        offset += line.len();
    }
    start.map(|start| (start, raw.len()))
}

fn top_level_key(line: &str) -> bool {
    !line.is_empty()
        && !line.starts_with([' ', '\t', '#', '-'])
        && line.find(':').is_some_and(|colon| {
            let rest = &line[colon + 1..];
            rest.is_empty() || rest.starts_with([' ', '\t', '\r'])
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_section_references_ignore_quoted_markers() {
        assert!(top_level_section_has_references(
            "skills:\n  config: *defaults\nother: true\n",
            "skills"
        ));
        assert!(top_level_section_has_references(
            "skills:\n  <<: { disabled: [Docs] }\n",
            "skills"
        ));
        assert!(!top_level_section_has_references(
            "skills:\n  note: 'literal * value'\nother: &outside value\n",
            "skills"
        ));
    }
}
