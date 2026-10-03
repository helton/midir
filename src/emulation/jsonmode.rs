//! Structured output for text backends: parse the model's text as JSON (tolerating code
//! fences) and validate it against the schema.

use serde_json::Value;

use super::parser::strip_fences;
use crate::py::json as pyjson;
use crate::py::text;

fn matches_type(value: &Value, t: &Value) -> bool {
    let Value::String(t) = t else { return true }; // py.get(t, object): any value is an object
    match t.as_str() {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => text::is_int(value),
        "number" => value.is_number() || value.is_boolean(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// `value in container` for the `enum` check (Python semantics).
fn contains(container: &Value, value: &Value) -> bool {
    match container {
        Value::Array(a) => a.iter().any(|x| text::eq(x, value)),
        Value::Object(m) => value.as_str().map_or(false, |k| m.contains_key(k)),
        Value::String(s) => value.as_str().map_or(false, |v| s.contains(v)),
        _ => false,
    }
}

/// Basic validation (type, required, properties, items, enum, additionalProperties).
/// CAVEAT: allOf/oneOf/anyOf, pattern and format are not checked.
pub fn validate_json_schema(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut errs = vec![];
    let typ = schema.get("type").cloned().unwrap_or(Value::Null);
    let types: Vec<Value> = match &typ {
        Value::Array(a) => a.clone(),
        t if text::truthy(t) => vec![t.clone()],
        _ => vec![],
    };
    if !types.is_empty()
        && !types.iter().any(|t| matches_type(value, t) && !(text::eq(t, &Value::String("integer".into())) && value.is_boolean()))
    {
        return vec![format!("{path}: expected {}, got {}", text::str_of(&typ), text::type_name(value))];
    }
    if let Some(e) = schema.get("enum") {
        if !contains(e, value) {
            errs.push(format!("{path}: value not in enum"));
        }
    }
    if let Value::Object(obj) = value {
        if let Some(req) = schema.get("required") {
            if let Ok(items) = crate::py::obj::iter(req) {
                for k in items {
                    let key = text::str_of(&k);
                    if !(k.is_string() && obj.contains_key(&key)) {
                        errs.push(format!("{path}.{key}: required property missing"));
                    }
                }
            }
        }
        if let Some(Value::Object(props)) = schema.get("properties") {
            for (k, sub) in props {
                if let (Some(v), true) = (obj.get(k), sub.is_object()) {
                    errs.extend(validate_json_schema(v, sub, &format!("{path}.{k}")));
                }
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let props = schema.get("properties").and_then(Value::as_object);
            let mut extra: Vec<String> = obj.keys().filter(|k| props.map_or(true, |p| !p.contains_key(*k))).cloned().collect();
            extra.sort();
            if !extra.is_empty() {
                errs.push(format!("{path}: additional properties not allowed {}", text::repr_list(&extra)));
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
pub fn check_json(text_: &str, schema: &Value) -> (Option<String>, Vec<String>) {
    let value = match pyjson::loads(&strip_fences(text::strip(text_))) {
        Ok(v) => v,
        Err(e) => return (None, vec![format!("not JSON: {e}")]),
    };
    let errs = if text::truthy(schema) { validate_json_schema(&value, schema, "$") } else { vec![] };
    if errs.is_empty() {
        (Some(pyjson::dumps(&value, pyjson::DEFAULT)), errs)
    } else {
        (None, errs)
    }
}
