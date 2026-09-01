//! Round-trip JSON5 object updates that preserve unrelated source text.

use json_five::rt::parser::{
    from_str as parse_round_trip_json5, JSONKeyValuePair, JSONObjectContext, JSONText, JSONValue,
    KeyValuePairContext,
};
use serde_json::{Map, Value};

pub(crate) fn replace_top_level_value(
    source: &str,
    key: &str,
    value: &Value,
) -> Result<String, String> {
    replace_object_path_value(source, &[key], value)
}

pub(crate) fn replace_object_path_value(
    source: &str,
    path: &[&str],
    value: &Value,
) -> Result<String, String> {
    if path.is_empty() {
        return Err("JSON5 object path is empty".to_owned());
    }
    let mut text: JSONText = parse_round_trip_json5(source)
        .map_err(|_| "round-trip JSON5 could not be parsed".to_owned())?;
    replace_path(&mut text.value, path, value)?;
    Ok(text.to_string())
}

pub(crate) fn object_path_has_comments(source: &str, path: &[&str]) -> Result<bool, String> {
    if path.is_empty() {
        return Err("JSON5 object path is empty".to_owned());
    }
    let text: JSONText = parse_round_trip_json5(source)
        .map_err(|_| "round-trip JSON5 could not be parsed".to_owned())?;
    Ok(find_path(&text.value, path)?
        .map(ToString::to_string)
        .is_some_and(|value| contains_comment(&value)))
}

fn find_path<'a>(node: &'a JSONValue, path: &[&str]) -> Result<Option<&'a JSONValue>, String> {
    let JSONValue::JSONObject {
        key_value_pairs, ..
    } = node
    else {
        return Err(format!("'{}' parent must be an object", path[0]));
    };
    let mut matches = key_value_pairs
        .iter()
        .filter(|pair| key_name(&pair.key) == Some(path[0]));
    let Some(pair) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(format!("contains duplicate '{}' fields", path[0]));
    }
    if path.len() == 1 {
        Ok(Some(&pair.value))
    } else {
        find_path(&pair.value, &path[1..])
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
            continue;
        }
        if character == '/'
            && characters
                .peek()
                .is_some_and(|next| matches!(next, '/' | '*'))
        {
            return true;
        }
    }
    false
}

fn replace_path(node: &mut JSONValue, path: &[&str], value: &Value) -> Result<(), String> {
    let JSONValue::JSONObject {
        key_value_pairs,
        context,
    } = node
    else {
        return Err(format!("'{}' parent must be an object", path[0]));
    };
    let matches = key_value_pairs
        .iter()
        .enumerate()
        .filter_map(|(index, pair)| (key_name(&pair.key) == Some(path[0])).then_some(index))
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(format!("contains duplicate '{}' fields", path[0]));
    }
    if matches.is_empty() {
        ensure_object_context(key_value_pairs, context);
    }
    if path.len() > 1 {
        if let Some(index) = matches.first().copied() {
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
    if let Some(index) = matches.first().copied() {
        key_value_pairs[index].value = value;
    } else {
        insert_entry(key_value_pairs, context, path[0], value);
    }
    Ok(())
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
    parse_round_trip_json5(&adjusted)
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
    let mut chars = key.chars();
    let identifier = chars
        .next()
        .is_some_and(|first| matches!(first, 'a'..='z' | 'A'..='Z' | '_' | '$'))
        && chars
            .all(|character| matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$'));
    if identifier {
        JSONValue::Identifier(key.to_owned())
    } else {
        JSONValue::DoubleQuotedString(key.to_owned())
    }
}

fn key_name(key: &JSONValue) -> Option<&str> {
    match key {
        JSONValue::Identifier(value)
        | JSONValue::DoubleQuotedString(value)
        | JSONValue::SingleQuotedString(value) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn nested_update_preserves_unrelated_comments() {
        let source = "{\n  // keep\n  theme: 'dark',\n  skills: {\n    // keep paths\n    paths: ['team'],\n    disabled: ['old'],\n  },\n}\n";
        let output =
            replace_object_path_value(source, &["skills", "disabled"], &json!(["old", "docs"]))
                .unwrap();

        assert!(output.contains("// keep\n"));
        assert!(output.contains("// keep paths\n"));
        assert!(output.contains("theme: 'dark'"));
        let parsed: Value = json5::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["disabled"], json!(["old", "docs"]));
    }

    #[test]
    fn nested_update_creates_missing_objects() {
        let output = replace_object_path_value(
            "{ theme: 'dark' }",
            &["skills", "disabled"],
            &json!(["docs"]),
        )
        .unwrap();
        let parsed: Value = json5::from_str(&output).unwrap();
        assert_eq!(parsed["skills"]["disabled"], json!(["docs"]));
        assert_eq!(parsed["theme"], "dark");
    }

    #[test]
    fn comments_are_detected_only_inside_the_selected_value() {
        let source = "{\n  // keep\n  skills: { disabled: ['old', // policy\n  ] },\n}\n";
        assert!(object_path_has_comments(source, &["skills", "disabled"]).unwrap());
        assert!(!object_path_has_comments(
            "{ // keep\n skills: { disabled: ['old'] } }",
            &["skills", "disabled"]
        )
        .unwrap());
        assert!(!object_path_has_comments(source, &["missing"]).unwrap());
    }
}
