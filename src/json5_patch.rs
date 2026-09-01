//! Focused JSON5 object updates that preserve unrelated source text.

use json_five::rt::parser::{
    from_str as parse_json5, JSONKeyValuePair, JSONObjectContext, JSONText, JSONValue,
    KeyValuePairContext,
};
use serde_json::{Map, Value};

pub(crate) fn replace_object_path_value(
    source: &str,
    path: &[&str],
    value: &Value,
) -> Result<String, String> {
    if path.is_empty() {
        return Err("JSON5 object path is empty".to_owned());
    }
    let mut document: JSONText =
        parse_json5(source).map_err(|_| "round-trip JSON5 could not be parsed".to_owned())?;
    replace_path(&mut document.value, path, value)?;
    Ok(document.to_string())
}

pub(crate) fn object_path_has_comments(source: &str, path: &[&str]) -> Result<bool, String> {
    if path.is_empty() {
        return Err("JSON5 object path is empty".to_owned());
    }
    let document: JSONText =
        parse_json5(source).map_err(|_| "round-trip JSON5 could not be parsed".to_owned())?;
    Ok(find_path(&document.value, path)?.is_some_and(value_has_comments))
}

fn find_path<'a>(node: &'a JSONValue, path: &[&str]) -> Result<Option<&'a JSONValue>, String> {
    let JSONValue::JSONObject {
        key_value_pairs, ..
    } = node
    else {
        return Err(format!("'{}' parent must be an object", path[0]));
    };
    let matches = matching_keys(key_value_pairs, path[0])?;
    let Some(index) = unique_index(matches, path[0])? else {
        return Ok(None);
    };
    if path.len() == 1 {
        Ok(Some(&key_value_pairs[index].value))
    } else {
        find_path(&key_value_pairs[index].value, &path[1..])
    }
}

fn replace_path(node: &mut JSONValue, path: &[&str], value: &Value) -> Result<(), String> {
    let JSONValue::JSONObject {
        key_value_pairs,
        context,
    } = node
    else {
        return Err(format!("'{}' parent must be an object", path[0]));
    };
    let index = unique_index(matching_keys(key_value_pairs, path[0])?, path[0])?;
    if index.is_none() {
        ensure_object_context(key_value_pairs, context);
    }
    if path.len() > 1 {
        if let Some(index) = index {
            return replace_path(&mut key_value_pairs[index].value, &path[1..], value);
        }
        let nested = nested_value(&path[1..], value.clone());
        let indent = object_indent(context);
        let value = round_trip_value(&nested, &indent)?;
        insert_entry(key_value_pairs, context, path[0], value);
        return Ok(());
    }

    let indent = object_indent(context);
    let value = round_trip_value(value, &indent)?;
    if let Some(index) = index {
        key_value_pairs[index].value = value;
    } else {
        insert_entry(key_value_pairs, context, path[0], value);
    }
    Ok(())
}

fn matching_keys(pairs: &[JSONKeyValuePair], expected: &str) -> Result<Vec<usize>, String> {
    pairs
        .iter()
        .enumerate()
        .filter_map(|(index, pair)| match decoded_key(&pair.key) {
            Ok(key) if key == expected => Some(Ok(index)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn unique_index(indices: Vec<usize>, key: &str) -> Result<Option<usize>, String> {
    if indices.len() > 1 {
        Err(format!("contains duplicate '{key}' fields"))
    } else {
        Ok(indices.into_iter().next())
    }
}

fn decoded_key(key: &JSONValue) -> Result<String, String> {
    match key {
        JSONValue::Identifier(value) => Ok(value.clone()),
        JSONValue::DoubleQuotedString(_) | JSONValue::SingleQuotedString(_) => {
            json5::from_str::<String>(&key.to_string())
                .map_err(|_| "JSON5 object key could not be decoded".to_owned())
        }
        _ => Err("JSON5 object contains an invalid key".to_owned()),
    }
}

fn nested_value(path: &[&str], value: Value) -> Value {
    path.iter().rev().fold(value, |value, key| {
        let mut object = Map::new();
        object.insert((*key).to_owned(), value);
        Value::Object(object)
    })
}

fn object_indent(context: &Option<JSONObjectContext>) -> String {
    context
        .as_ref()
        .map(|context| trailing_indent(&context.wsc.0))
        .unwrap_or_default()
}

fn insert_entry(
    pairs: &mut Vec<JSONKeyValuePair>,
    context: &mut Option<JSONObjectContext>,
    key: &str,
    value: JSONValue,
) {
    ensure_object_context(pairs, context);
    let leading = context
        .as_ref()
        .map(|context| context.wsc.0.clone())
        .unwrap_or_default();
    let separator = if leading.contains('\n') {
        format!("\n{}", trailing_indent(&leading))
    } else {
        String::new()
    };
    let closing = if let Some(last) = pairs.last_mut() {
        let context = last.context.get_or_insert_with(|| KeyValuePairContext {
            wsc: (String::new(), " ".to_owned(), String::new(), None),
        });
        if let Some(after_comma) = context.wsc.3.clone() {
            context.wsc.3 = Some(separator);
            after_comma
        } else {
            let closing = std::mem::take(&mut context.wsc.2);
            context.wsc.3 = Some(separator);
            closing
        }
    } else {
        closing_whitespace(&leading)
    };
    pairs.push(JSONKeyValuePair {
        key: json5_key(key),
        value,
        context: Some(KeyValuePairContext {
            wsc: (String::new(), " ".to_owned(), closing, None),
        }),
    });
}

fn ensure_object_context(pairs: &[JSONKeyValuePair], context: &mut Option<JSONObjectContext>) {
    if pairs.is_empty()
        && context
            .as_ref()
            .is_none_or(|context| context.wsc.0.is_empty())
    {
        *context = Some(JSONObjectContext {
            wsc: ("\n  ".to_owned(),),
        });
    }
}

fn round_trip_value(value: &Value, parent_indent: &str) -> Result<JSONValue, String> {
    let source = serde_json::to_string_pretty(value)
        .map_err(|error| format!("JSON value could not be serialized: {error}"))?;
    let adjusted = if parent_indent.is_empty() || !source.contains('\n') {
        source
    } else {
        let mut lines = source.lines();
        let mut adjusted = lines.next().unwrap_or_default().to_owned();
        for line in lines {
            adjusted.push('\n');
            adjusted.push_str(parent_indent);
            adjusted.push_str(line);
        }
        adjusted
    };
    parse_json5(&adjusted)
        .map(|text| text.value)
        .map_err(|_| "JSON value could not be projected".to_owned())
}

fn trailing_indent(value: &str) -> String {
    value
        .rsplit_once('\n')
        .map(|(_, indent)| indent.to_owned())
        .unwrap_or_default()
}

fn closing_whitespace(value: &str) -> String {
    let Some((prefix, indent)) = value.rsplit_once('\n') else {
        return String::new();
    };
    let indent = indent
        .strip_suffix('\t')
        .or_else(|| indent.strip_suffix("  "))
        .or_else(|| indent.strip_suffix(' '))
        .unwrap_or(indent);
    format!("{prefix}\n{indent}")
}

fn json5_key(key: &str) -> JSONValue {
    let mut characters = key.chars();
    let identifier = characters
        .next()
        .is_some_and(|first| matches!(first, 'a'..='z' | 'A'..='Z' | '_' | '$'))
        && characters
            .all(|character| matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$'));
    if identifier {
        JSONValue::Identifier(key.to_owned())
    } else {
        JSONValue::DoubleQuotedString(key.to_owned())
    }
}

fn contains_comment(source: &str) -> bool {
    let mut characters = source.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '/'
            && characters
                .peek()
                .is_some_and(|next| matches!(next, '/' | '*'))
        {
            return true;
        }
    }
    false
}

fn value_has_comments(value: &JSONValue) -> bool {
    match value {
        JSONValue::JSONObject {
            key_value_pairs,
            context,
        } => {
            context
                .as_ref()
                .is_some_and(|context| contains_comment(&context.wsc.0))
                || key_value_pairs.iter().any(|pair| {
                    pair.context.as_ref().is_some_and(|context| {
                        contains_comment(&context.wsc.0)
                            || contains_comment(&context.wsc.1)
                            || contains_comment(&context.wsc.2)
                            || context.wsc.3.as_deref().is_some_and(contains_comment)
                    }) || value_has_comments(&pair.value)
                })
        }
        JSONValue::JSONArray { values, context } => {
            context
                .as_ref()
                .is_some_and(|context| contains_comment(&context.wsc.0))
                || values.iter().any(|value| {
                    value.context.as_ref().is_some_and(|context| {
                        contains_comment(&context.wsc.0)
                            || context.wsc.1.as_deref().is_some_and(contains_comment)
                    }) || value_has_comments(&value.value)
                })
        }
        JSONValue::Unary { value, .. } => value_has_comments(value),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn nested_update_preserves_unrelated_json5_text() {
        let source = "{\n  // keep\n  theme: 'dark',\n  skills: { disabled: ['old'] },\n}\n";
        let output =
            replace_object_path_value(source, &["skills", "disabled"], &json!(["docs"])).unwrap();

        assert!(output.contains("// keep"));
        assert!(output.contains("theme: 'dark'"));
        let parsed: Value = json5::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["disabled"], json!(["docs"]));
    }

    #[test]
    fn escaped_duplicate_keys_are_rejected() {
        let source = r#"{ skills: {}, "sk\u0069lls": {} }"#;
        assert!(replace_object_path_value(source, &["skills", "disabled"], &json!([])).is_err());
    }

    #[test]
    fn comments_inside_the_replaced_value_are_detected() {
        let source = "{ skills: { disabled: ['old', // keep\n] } }";
        assert!(object_path_has_comments(source, &["skills", "disabled"]).unwrap());
        assert!(
            !object_path_has_comments("{ // keep\n skills: {} }", &["skills", "disabled"]).unwrap()
        );
    }
}
