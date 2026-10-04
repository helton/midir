//! Structured output for text backends: parse the model's text as JSON (tolerating code fences) and validate it against
//! the schema.

use serde_json::Value;

use super::parser::strip_fences;

/// The JSON Schema type name of a value.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

fn matches_type(value: &Value, t: &str) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => is_integer(value),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true, // an unknown type name constrains nothing
    }
}

/// JSON Schema equality: numbers by value (1 == 1.0), objects regardless of key order.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w))),
        _ => a == b,
    }
}

/// Basic validation (type, required, properties, items, enum, additionalProperties).
/// CAVEAT: allOf/oneOf/anyOf, pattern and format are not checked.
pub fn validate_json_schema(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !types.is_empty() && !types.iter().any(|t| matches_type(value, t)) {
        return vec![format!("{path}: expected {}, got {}", types.join(" or "), type_name(value))];
    }
    let mut errs = vec![];
    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options.iter().any(|o| json_eq(o, value)) {
            errs.push(format!("{path}: value not in enum"));
        }
    }
    if let Value::Object(obj) = value {
        if let Some(Value::Array(required)) = schema.get("required") {
            for key in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(key) {
                    errs.push(format!("{path}.{key}: required property missing"));
                }
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (k, sub) in props.into_iter().flatten() {
            if let (Some(v), true) = (obj.get(k), sub.is_object()) {
                errs.extend(validate_json_schema(v, sub, &format!("{path}.{k}")));
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let mut extra: Vec<&str> = obj.keys().map(String::as_str).filter(|k| props.map_or(true, |p| !p.contains_key(*k))).collect();
            extra.sort_unstable();
            if !extra.is_empty() {
                errs.push(format!("{path}: additional properties not allowed ({})", extra.join(", ")));
            }
        }
    }
    if let (Value::Array(items), Some(sub @ Value::Object(_))) = (value, schema.get("items")) {
        for (i, v) in items.iter().enumerate() {
            errs.extend(validate_json_schema(v, sub, &format!("{path}[{i}]")));
        }
    }
    errs
}

/// Parse the model output as JSON (tolerating code fences) and validate it. Returns (normalized JSON, errors).
pub fn check_json(text: &str, schema: &Value) -> (Option<String>, Vec<String>) {
    let value: Value = match serde_json::from_str(&strip_fences(text.trim())) {
        Ok(v) => v,
        Err(e) => return (None, vec![format!("not JSON: {e}")]),
    };
    let errs = validate_json_schema(&value, schema, "$");
    if errs.is_empty() {
        (Some(value.to_string()), errs)
    } else {
        (None, errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validation() {
        let schema = json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "integer", "enum": [1, 2]}}, "additionalProperties": false});
        assert!(validate_json_schema(&json!({"a": 1.0}), &schema, "$").is_empty());
        assert_eq!(validate_json_schema(&json!({"a": "x"}), &schema, "$"), vec!["$.a: expected integer, got string"]);
        assert_eq!(
            validate_json_schema(&json!({"b": 1}), &schema, "$"),
            vec!["$.a: required property missing", "$: additional properties not allowed (b)"]
        );
        assert_eq!(check_json("```json\n{\"a\": 2}\n```", &schema).0.as_deref(), Some("{\"a\":2}"));
        assert!(check_json("nope", &schema).1[0].starts_with("not JSON: "));
    }
}
