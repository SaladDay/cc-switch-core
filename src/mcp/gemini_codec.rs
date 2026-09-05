use serde_json::{Map, Value};

pub(super) enum TypeInference {
    FieldPresence,
    StringFields,
}

pub(super) fn decode_fields(output: &mut Map<String, Value>, inference: TypeInference) {
    if let Some(url) = output.remove("httpUrl") {
        output.insert("url".to_owned(), url);
        output.insert("type".to_owned(), Value::String("http".to_owned()));
        return;
    }
    match inference {
        TypeInference::FieldPresence => super::infer_transport(output),
        TypeInference::StringFields => {
            if output.get("type").is_some_and(Value::is_string) {
                return;
            }
            let typ = if output.get("command").is_some_and(Value::is_string) {
                "stdio"
            } else if output.get("url").is_some_and(Value::is_string) {
                "sse"
            } else {
                return;
            };
            output.insert("type".to_owned(), Value::String(typ.to_owned()));
        }
    }
}

pub(super) fn encode_preserving_fields(server: &Map<String, Value>) -> Value {
    let mut output = server.clone();
    if output.get("type").and_then(Value::as_str) == Some("http") {
        if let Some(url) = output.remove("url") {
            output.insert("httpUrl".to_owned(), url);
        }
    }

    let existing = output
        .get("timeout")
        .and_then(|value| numeric_timeout(value, 1));
    let startup = take_timeout(&mut output, "startup_timeout_sec", 1_000)
        .or_else(|| take_timeout(&mut output, "startup_timeout_ms", 1))
        .unwrap_or(10_000);
    let tool = take_timeout(&mut output, "tool_timeout_sec", 1_000)
        .or_else(|| take_timeout(&mut output, "tool_timeout_ms", 1))
        .unwrap_or(60_000);
    output.insert(
        "timeout".to_owned(),
        Value::Number(existing.unwrap_or(0).max(startup).max(tool).into()),
    );
    output.remove("type");
    Value::Object(output)
}

fn take_timeout(output: &mut Map<String, Value>, key: &str, multiplier: u64) -> Option<u64> {
    output
        .remove(key)
        .and_then(|value| numeric_timeout(&value, multiplier))
}

fn numeric_timeout(value: &Value, multiplier: u64) -> Option<u64> {
    value
        .as_u64()
        .map(|number| number.saturating_mul(multiplier))
        .or_else(|| {
            value
                .as_f64()
                .map(|number| (number * multiplier as f64) as u64)
        })
}
