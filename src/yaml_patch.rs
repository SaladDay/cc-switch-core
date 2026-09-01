//! Conservative YAML edits that preserve every byte outside the selected
//! mapping field.

pub(crate) fn replace_top_level_string_sequence(
    raw: &str,
    section_key: &str,
    field_key: &str,
    values: &[String],
    section_existed: bool,
) -> Result<String, String> {
    let line_ending = preferred_line_ending(raw);
    let Some((section_start, section_end)) = section_range(raw, section_key) else {
        if section_existed || uses_flow_root(raw) {
            return Err(format!(
                "existing '{section_key}' section cannot be updated without rewriting unrelated YAML"
            ));
        }
        let insertion = document_end_offset(raw).unwrap_or(raw.len());
        let rendered = render_section(section_key, field_key, values, line_ending)?;
        return insert_at(raw, insertion, &rendered, line_ending);
    };

    let header_end = line_end(raw, section_start);
    let header = raw[section_start..header_end].trim_end_matches(['\r', '\n']);
    let suffix = header
        .strip_prefix(&format!("{section_key}:"))
        .ok_or_else(|| format!("'{section_key}' section header is ambiguous"))?
        .trim();
    if matches!(suffix, "{}" | "null" | "Null" | "NULL" | "~") {
        let rendered = render_section(section_key, field_key, values, line_ending)?;
        return replace_range(raw, section_start, header_end, &rendered, line_ending);
    }
    if !suffix.is_empty() {
        return Err(format!(
            "'{section_key}' uses an inline value that cannot be updated safely"
        ));
    }

    let body_start = header_end;
    let lines = line_spans(raw, body_start, section_end);
    let child_indent = lines
        .iter()
        .filter(|line| !line.plain.trim().is_empty())
        .map(|line| indentation(line.plain))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .unwrap_or(2);
    if child_indent == 0 {
        return Err(format!(
            "'{section_key}' section is not an indented mapping"
        ));
    }

    let mut field_index = None;
    for (index, line) in lines.iter().enumerate() {
        if line.plain.trim().is_empty() || indentation(line.plain)? != child_indent {
            continue;
        }
        let entry = &line.plain[child_indent..];
        if is_indentless_sequence_item(entry) {
            continue;
        }
        let Some(decoded) = decoded_top_level_key(entry) else {
            return Err(format!(
                "'{section_key}' contains a mapping entry that cannot be preserved safely"
            ));
        };
        if decoded != field_key {
            continue;
        }
        if !entry
            .strip_prefix(field_key)
            .is_some_and(|rest| rest.starts_with(':'))
            || field_index.replace(index).is_some()
        {
            return Err(format!(
                "'{section_key}.{field_key}' is duplicate or ambiguously spelled"
            ));
        }
    }

    let rendered = render_field(child_indent, field_key, values, line_ending)?;
    if let Some(index) = field_index {
        let start = lines[index].start;
        let mut end = lines[index].end;
        for line in &lines[index + 1..] {
            if line.plain.trim().is_empty() {
                continue;
            }
            let indent = indentation(line.plain)?;
            if indent < child_indent
                || (indent == child_indent && !is_indentless_sequence_item(&line.plain[indent..]))
            {
                break;
            }
            end = line.end;
        }
        replace_range(raw, start, end, &rendered, line_ending)
    } else {
        let insertion = lines
            .iter()
            .rev()
            .find(|line| !line.plain.trim().is_empty())
            .map(|line| line.end)
            .unwrap_or(body_start);
        insert_at(raw, insertion, &rendered, line_ending)
    }
}

fn is_indentless_sequence_item(line: &str) -> bool {
    line.strip_prefix('-')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

struct LineSpan<'a> {
    start: usize,
    end: usize,
    plain: &'a str,
}

fn line_spans(raw: &str, start: usize, end: usize) -> Vec<LineSpan<'_>> {
    let mut lines = Vec::new();
    let mut offset = start;
    for line in raw[start..end].split_inclusive('\n') {
        let next = offset + line.len();
        lines.push(LineSpan {
            start: offset,
            end: next,
            plain: line.trim_end_matches(['\r', '\n']),
        });
        offset = next;
    }
    lines
}

fn indentation(line: &str) -> Result<usize, String> {
    let prefix = line
        .find(|character: char| !character.is_whitespace())
        .unwrap_or(line.len());
    if !line[..prefix].bytes().all(|byte| byte == b' ') {
        Err("YAML mapping indentation must contain only spaces".to_owned())
    } else {
        Ok(prefix)
    }
}

fn render_section(
    section_key: &str,
    field_key: &str,
    values: &[String],
    line_ending: &str,
) -> Result<String, String> {
    let mut rendered = format!("{section_key}:{line_ending}");
    rendered.push_str(&render_field(2, field_key, values, line_ending)?);
    Ok(rendered)
}

fn render_field(
    indent: usize,
    field_key: &str,
    values: &[String],
    line_ending: &str,
) -> Result<String, String> {
    let prefix = " ".repeat(indent);
    if values.is_empty() {
        return Ok(format!("{prefix}{field_key}: []{line_ending}"));
    }
    let mut rendered = format!("{prefix}{field_key}:{line_ending}");
    for value in values {
        let scalar = serde_yaml::to_string(&serde_yaml::Value::String(value.clone()))
            .map_err(|error| format!("YAML string could not be serialized: {error}"))?;
        let scalar = scalar.trim_end_matches(['\r', '\n']);
        if scalar.contains('\n') {
            return Err("YAML string requires a multiline representation".to_owned());
        }
        rendered.push_str(&prefix);
        rendered.push_str("  - ");
        rendered.push_str(scalar);
        rendered.push_str(line_ending);
    }
    Ok(rendered)
}

fn replace_range(
    raw: &str,
    start: usize,
    end: usize,
    rendered: &str,
    line_ending: &str,
) -> Result<String, String> {
    let keep_trailing_line_ending = raw[start..end].ends_with('\n') || end < raw.len();
    let rendered = if keep_trailing_line_ending {
        rendered.to_owned()
    } else {
        rendered
            .strip_suffix(line_ending)
            .unwrap_or(rendered)
            .to_owned()
    };
    let mut output = String::with_capacity(raw.len() + rendered.len());
    output.push_str(&raw[..start]);
    output.push_str(&rendered);
    output.push_str(&raw[end..]);
    Ok(output)
}

fn insert_at(
    raw: &str,
    offset: usize,
    rendered: &str,
    line_ending: &str,
) -> Result<String, String> {
    if !raw.is_char_boundary(offset) {
        return Err("YAML insertion point is not a character boundary".to_owned());
    }
    let mut output = String::with_capacity(raw.len() + rendered.len() + line_ending.len());
    output.push_str(&raw[..offset]);
    if offset > 0 && !raw[..offset].ends_with('\n') {
        output.push_str(line_ending);
    }
    output.push_str(rendered);
    output.push_str(&raw[offset..]);
    Ok(output)
}

fn preferred_line_ending(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn line_end(raw: &str, start: usize) -> usize {
    raw[start..]
        .find('\n')
        .map_or(raw.len(), |relative| start + relative + 1)
}

fn document_end_offset(raw: &str) -> Option<usize> {
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        let plain = line.trim_end_matches(['\r', '\n']);
        if document_boundary(plain) && plain.trim_start().starts_with("...") {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

pub(crate) fn has_duplicate_top_level_key(raw: &str, key: &str) -> bool {
    let mut found = false;
    for line in raw.lines() {
        if decoded_top_level_key(line).as_deref() != Some(key) {
            continue;
        }
        let canonical = line
            .strip_prefix(key)
            .is_some_and(|suffix| suffix.starts_with(':'));
        if found || !canonical {
            return true;
        }
        found = true;
    }
    false
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
    let code = scan_yaml_line(line).0;
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

fn line_has_comment(line: &str) -> bool {
    scan_yaml_line(line).1
}

fn scan_yaml_line(line: &str) -> (String, bool) {
    let mut output = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    let mut previous = None;
    let mut last_non_whitespace = None;
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
                    last_non_whitespace = Some(character);
                }
            }
        } else if matches!(character, '\'' | '"') && quoted_scalar_can_start(last_non_whitespace) {
            quote = Some(character);
            output.push(' ');
        } else if character == '#' && previous.is_none_or(char::is_whitespace) {
            return (output, true);
        } else {
            output.push(character);
            if !character.is_whitespace() {
                last_non_whitespace = Some(character);
            }
        }
        previous = Some(character);
    }
    (output, false)
}

fn quoted_scalar_can_start(last_non_whitespace: Option<char>) -> bool {
    last_non_whitespace
        .is_none_or(|character| matches!(character, ':' | '-' | '?' | '[' | '{' | ','))
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
        } else if start.is_some() && (top_level_key(plain) || document_boundary(plain)) {
            return Some((start.expect("section start"), offset));
        }
        offset += line.len();
    }
    start.map(|start| (start, raw.len()))
}

fn document_boundary(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    let code = scan_yaml_line(line).0;
    matches!(code.trim(), "---" | "...")
}

fn top_level_key(line: &str) -> bool {
    !line.is_empty()
        && !line.starts_with([' ', '\t', '#', '-'])
        && top_level_colon(line).is_some_and(|colon| {
            let rest = &line[colon + 1..];
            rest.is_empty() || rest.starts_with([' ', '\t', '\r'])
        })
}

fn decoded_top_level_key(line: &str) -> Option<String> {
    if !top_level_key(line) {
        return None;
    }
    let colon = top_level_colon(line)?;
    serde_yaml::from_str::<String>(line[..colon].trim_end()).ok()
}

fn top_level_colon(line: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    let mut characters = line.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if active_quote == '"' && character == '\\' {
                escaped = true;
            } else if character == active_quote {
                if active_quote == '\'' && characters.peek().is_some_and(|(_, next)| *next == '\'')
                {
                    characters.next();
                } else {
                    quote = None;
                }
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == ':' {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_references_ignore_quoted_markers() {
        assert!(top_level_section_has_references(
            "skills:\n  config: *defaults\nother: true\n",
            "skills"
        ));
        assert!(!top_level_section_has_references(
            "skills:\n  note: 'literal * value'\nother: &outside value\n",
            "skills"
        ));
    }

    #[test]
    fn comments_are_scoped_to_the_selected_section() {
        assert!(top_level_section_has_comments(
            "skills:\n  disabled: [] # keep\nother: true\n",
            "skills"
        ));
        assert!(!top_level_section_has_comments(
            "skills:\n  note: 'literal # value'\nother: true # keep\n",
            "skills"
        ));
    }

    #[test]
    fn quoted_or_duplicate_target_keys_are_ambiguous() {
        assert!(has_duplicate_top_level_key(
            "skills: {}\n\"skills\": {}\n",
            "skills"
        ));
        assert!(has_duplicate_top_level_key("'skills': {}\n", "skills"));
        assert!(!has_duplicate_top_level_key(
            "skills: {}\nother: {}\n",
            "skills"
        ));
    }

    #[test]
    fn field_update_preserves_unknown_text_and_document_end() {
        let source =
            "model:\n  default: test\nskills:\n  disabled: [old]\n  custom: \"001\"\n...\n";
        let output = replace_top_level_string_sequence(
            source,
            "skills",
            "disabled",
            &["demo".to_owned()],
            true,
        )
        .unwrap();

        assert!(output.starts_with("model:\n  default: test\nskills:\n"));
        assert!(output.ends_with("  custom: \"001\"\n...\n"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["disabled"][0], "demo");
        assert_eq!(parsed["skills"]["custom"], "001");
    }

    #[test]
    fn flow_mapping_with_unknown_fields_is_rejected() {
        let source = "skills: {disabled: [], custom: \"001\"}\n...\n";
        assert!(replace_top_level_string_sequence(
            source,
            "skills",
            "disabled",
            &["demo".to_owned()],
            true,
        )
        .is_err());
    }

    #[test]
    fn indentless_sequences_are_updated_without_consuming_the_next_field() {
        let source = "skills:\n  disabled:\n  - old\n  paths:\n  - team\n";
        let output = replace_top_level_string_sequence(
            source,
            "skills",
            "disabled",
            &["demo".to_owned()],
            true,
        )
        .unwrap();

        assert!(output.ends_with("  paths:\n  - team\n"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["disabled"][0], "demo");
        assert_eq!(parsed["skills"]["paths"][0], "team");
    }
}
