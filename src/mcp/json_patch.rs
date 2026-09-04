use std::{collections::HashSet, ops::Range};

use serde_json::{value::RawValue, Map, Value};

struct Entry {
    key: String,
    value: Range<usize>,
}

pub(super) fn replace_top_level_value(
    original: &str,
    key: &str,
    value: &Value,
) -> Result<String, String> {
    let (entries, root_close) = root_entries(original)?;
    let matching = entries
        .iter()
        .filter(|entry| entry.key == key)
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(format!("contains duplicate '{key}' fields"));
    }
    let rendered = serde_json::to_string(value).map_err(|error| error.to_string())?;
    let mut output = original.to_owned();
    if let Some(entry) = matching.first() {
        output.replace_range(entry.value.clone(), &rendered);
        return Ok(output);
    }

    let rendered_key = serde_json::to_string(key).map_err(|error| error.to_string())?;
    let insertion = format!("{rendered_key}:{rendered}");
    if let Some(last) = entries.last() {
        output.insert_str(last.value.end, &format!(",{insertion}"));
    } else {
        output.insert_str(root_close, &insertion);
    }
    Ok(output)
}

pub(super) fn is_object(value: &str) -> bool {
    validate_object(value).is_ok()
}

pub(super) fn validate_object(value: &str) -> Result<(), String> {
    validate_json(value)?;
    root_entries(value).map(|_| ())
}

pub(super) fn object_entry(object: &str, key: &str) -> Result<Option<String>, String> {
    validate_object(object)?;
    let (entries, _) = root_entries(object)?;
    let matching = entries
        .iter()
        .filter(|entry| entry.key == key)
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(format!("contains duplicate '{key}' fields"));
    }
    Ok(matching
        .first()
        .map(|entry| object[entry.value.clone()].to_owned()))
}

pub(super) fn replace_nested_object_entry(
    original: &str,
    section: &str,
    key: &str,
    replacement: Option<&str>,
) -> Result<Option<String>, String> {
    validate_object(original)?;
    let section_value = object_entry(original, section)?;
    let section_source = section_value.as_deref().unwrap_or("{}");
    let Some(next_section) = patch_object_entry(section_source, key, replacement)? else {
        return Ok(None);
    };
    replace_top_level_raw(original, section, &next_section).map(Some)
}

pub(super) fn merge_object_fields(
    original: Option<&str>,
    clear_fields: &[&str],
    desired: &Map<String, Value>,
) -> Result<String, String> {
    let original = original.unwrap_or("{}");
    validate_object(original)?;
    let (entries, _) = root_entries(original)?;
    let clear = clear_fields.iter().copied().collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut fields = Vec::new();
    for entry in entries {
        if !seen.insert(entry.key.clone()) {
            return Err(format!("contains duplicate '{}' fields", entry.key));
        }
        if let Some(value) = desired.get(&entry.key) {
            fields.push((entry.key, serde_json::to_string(value).map_err(json_error)?));
        } else if !clear.contains(entry.key.as_str()) {
            fields.push((entry.key, original[entry.value].to_owned()));
        }
    }
    for (key, value) in desired {
        if seen.insert(key.clone()) {
            fields.push((
                key.clone(),
                serde_json::to_string(value).map_err(json_error)?,
            ));
        }
    }
    render_object(fields)
}

fn patch_object_entry(
    original: &str,
    key: &str,
    replacement: Option<&str>,
) -> Result<Option<String>, String> {
    validate_object(original)?;
    if let Some(replacement) = replacement {
        validate_json(replacement)?;
    }
    let (entries, _) = root_entries(original)?;
    let matching = entries.iter().filter(|entry| entry.key == key).count();
    if matching > 1 {
        return Err(format!("contains duplicate '{key}' fields"));
    }
    if matching == 0 && replacement.is_none() {
        return Ok(None);
    }
    let mut fields = Vec::with_capacity(entries.len() + usize::from(matching == 0));
    let mut inserted = false;
    for entry in entries {
        if entry.key == key {
            if let Some(replacement) = replacement {
                fields.push((entry.key, replacement.to_owned()));
                inserted = true;
            }
        } else {
            fields.push((entry.key, original[entry.value].to_owned()));
        }
    }
    if !inserted {
        if let Some(replacement) = replacement {
            fields.push((key.to_owned(), replacement.to_owned()));
        }
    }
    render_object(fields).map(Some)
}

fn replace_top_level_raw(original: &str, key: &str, value: &str) -> Result<String, String> {
    validate_json(value)?;
    let (entries, root_close) = root_entries(original)?;
    let matching = entries
        .iter()
        .filter(|entry| entry.key == key)
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(format!("contains duplicate '{key}' fields"));
    }
    let mut output = original.to_owned();
    if let Some(entry) = matching.first() {
        output.replace_range(entry.value.clone(), value);
        return Ok(output);
    }
    let rendered_key = serde_json::to_string(key).map_err(json_error)?;
    let insertion = format!("{rendered_key}:{value}");
    if let Some(last) = entries.last() {
        output.insert_str(last.value.end, &format!(",{insertion}"));
    } else {
        output.insert_str(root_close, &insertion);
    }
    Ok(output)
}

fn render_object(fields: Vec<(String, String)>) -> Result<String, String> {
    let mut output = String::from("{");
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&serde_json::to_string(&key).map_err(json_error)?);
        output.push(':');
        output.push_str(&value);
    }
    output.push('}');
    validate_json(&output)?;
    Ok(output)
}

fn validate_json(value: &str) -> Result<(), String> {
    RawValue::from_string(value.to_owned())
        .map(|_| ())
        .map_err(|_| "JSON could not be parsed".to_owned())
}

fn json_error(error: serde_json::Error) -> String {
    error.to_string()
}

fn root_entries(text: &str) -> Result<(Vec<Entry>, usize), String> {
    let bytes = text.as_bytes();
    let mut cursor = skip_whitespace(bytes, 0);
    if bytes.get(cursor) != Some(&b'{') {
        return Err("root must be an object".to_owned());
    }
    cursor += 1;
    let mut entries = Vec::new();
    loop {
        cursor = skip_whitespace(bytes, cursor);
        if bytes.get(cursor) == Some(&b'}') {
            let root_close = cursor;
            cursor = skip_whitespace(bytes, cursor + 1);
            if cursor != bytes.len() {
                return Err("JSON has trailing content".to_owned());
            }
            return Ok((entries, root_close));
        }

        let key_start = cursor;
        let key_end = string_end(bytes, key_start)?;
        let key = serde_json::from_slice::<String>(&bytes[key_start..key_end])
            .map_err(|_| "object key is invalid".to_owned())?;
        cursor = skip_whitespace(bytes, key_end);
        if bytes.get(cursor) != Some(&b':') {
            return Err("object key is missing ':'".to_owned());
        }
        cursor = skip_whitespace(bytes, cursor + 1);
        let value_start = cursor;
        let value_end = value_end(bytes, value_start)?;
        entries.push(Entry {
            key,
            value: value_start..value_end,
        });
        cursor = skip_whitespace(bytes, value_end);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => {}
            _ => return Err("object value is not followed by ',' or '}'".to_owned()),
        }
    }
}

fn skip_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    if bytes.get(start) != Some(&b'"') {
        return Err("object key must be a JSON string".to_owned());
    }
    let mut cursor = start + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor = cursor.saturating_add(2),
            b'"' => return Ok(cursor + 1),
            _ => cursor += 1,
        }
    }
    Err("JSON string is incomplete".to_owned())
}

fn value_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    match bytes.get(start) {
        Some(b'"') => string_end(bytes, start),
        Some(b'{') | Some(b'[') => composite_end(bytes, start),
        Some(_) => {
            let mut cursor = start;
            while !matches!(bytes.get(cursor), None | Some(b',') | Some(b'}')) {
                cursor += 1;
            }
            let end = bytes[..cursor]
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
                .map_or(start, |position| position + 1);
            if end == start {
                Err("object value is empty".to_owned())
            } else {
                Ok(end)
            }
        }
        None => Err("object value is missing".to_owned()),
    }
}

fn composite_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut stack = vec![match bytes[start] {
        b'{' => b'}',
        b'[' => b']',
        _ => unreachable!("composite starts with a container"),
    }];
    let mut cursor = start + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'"' => cursor = string_end(bytes, cursor)?,
            b'{' => {
                stack.push(b'}');
                cursor += 1;
            }
            b'[' => {
                stack.push(b']');
                cursor += 1;
            }
            closing if stack.last() == Some(closing) => {
                stack.pop();
                cursor += 1;
                if stack.is_empty() {
                    return Ok(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    Err("JSON container is incomplete".to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replaces_only_the_selected_top_level_value() {
        let original =
            "{\n  \"theme\" : \"dark\",\n  \"mcp\": { \"old\": true },\n  \"tail\" : [1, 2]\n}\n";
        let patched = replace_top_level_value(original, "mcp", &json!({"new": true})).unwrap();

        assert_eq!(
            patched,
            "{\n  \"theme\" : \"dark\",\n  \"mcp\": {\"new\":true},\n  \"tail\" : [1, 2]\n}\n"
        );
    }

    #[test]
    fn inserts_a_missing_value_without_rewriting_existing_data() {
        assert_eq!(
            replace_top_level_value("{\"keep\": 1}\n", "mcp", &json!({})).unwrap(),
            "{\"keep\": 1,\"mcp\":{}}\n"
        );
        assert_eq!(
            replace_top_level_value("{}", "mcp", &json!({})).unwrap(),
            "{\"mcp\":{}}"
        );
    }

    #[test]
    fn rejects_duplicate_selected_fields() {
        assert!(
            replace_top_level_value("{\"mcp\":{},\"mcp\":{}}", "mcp", &json!({}))
                .unwrap_err()
                .contains("duplicate")
        );
    }
}
