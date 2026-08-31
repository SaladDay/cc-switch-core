use std::ops::Range;

use serde_json::Value;

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
