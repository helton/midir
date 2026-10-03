//! Python object access on JSON values: `x.get(k)` (an AttributeError unless `x` is a dict), `for y in x` (dicts
//! iterate their keys, strings their characters, numbers raise TypeError), hashability. The protocol adapters use
//! these so malformed client input fails exactly where the Python implementation failed (a 400 "malformed request").

use serde_json::Value;

use super::text::type_name;

/// A Python exception raised by malformed input (AttributeError, TypeError, KeyError, ValueError).
#[derive(Debug, Clone)]
pub struct PyErr {
    pub kind: &'static str,
    pub msg: String,
}

impl PyErr {
    pub fn attr(v: &Value, attr: &str) -> Self {
        PyErr { kind: "AttributeError", msg: format!("'{}' object has no attribute '{attr}'", type_name(v)) }
    }

    pub fn type_err(msg: String) -> Self {
        PyErr { kind: "TypeError", msg }
    }
}

pub type PyResult<T> = Result<T, PyErr>;

/// `v.get(key)`: Some(Null) for an explicit null, None when absent.
pub fn get<'a>(v: &'a Value, key: &str) -> PyResult<Option<&'a Value>> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(PyErr::attr(other, "get")),
    }
}

/// `v.get(key) or {}` / `or []`: the value when truthy.
pub fn get_truthy<'a>(v: &'a Value, key: &str) -> PyResult<Option<&'a Value>> {
    Ok(get(v, key)?.filter(|x| super::text::truthy(x)))
}

/// `for x in v`.
pub fn iter(v: &Value) -> PyResult<Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(m) => Ok(m.keys().map(|k| Value::String(k.clone())).collect()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        other => Err(PyErr::type_err(format!("'{}' object is not iterable", type_name(other)))),
    }
}

/// `for x in (v or [])`.
pub fn iter_or_empty(v: Option<&Value>) -> PyResult<Vec<Value>> {
    match v {
        Some(x) if super::text::truthy(x) => iter(x),
        _ => Ok(Vec::new()),
    }
}

/// Dict keys and set members must be hashable.
pub fn hashable(v: &Value) -> PyResult<()> {
    match v {
        Value::Array(_) | Value::Object(_) => Err(PyErr::type_err(format!("unhashable type: '{}'", type_name(v)))),
        _ => Ok(()),
    }
}

/// `x == "literal"`.
pub fn is_str(v: Option<&Value>, s: &str) -> bool {
    matches!(v, Some(Value::String(x)) if x == s)
}
