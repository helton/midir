//! Compact tool listing (`tool_schema = "compact"`): each tool as a heading, its description and one line per
//! parameter (`name (type, required, constraints): description`, nested fields indented) instead of raw JSON Schema.
//! What the compact form cannot say (`$ref`, `allOf`, `propertyNames`, ...) is kept as JSON Schema, for that parameter
//! or, when it reaches the whole schema, for the whole tool.

use serde_json::{Map, Value};

use crate::canonical::ToolSpec;
use crate::text::ellipsize;

/// Keywords the compact form says, or drops because they tell the model nothing it needs to write arguments.
const KNOWN: [&str; 26] = [
    "type",
    "description",
    "properties",
    "required",
    "enum",
    "enumDescriptions",
    "items",
    "additionalProperties",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "multipleOf",
    "pattern",
    "format",
    "default",
    "const",
    "anyOf",
    "oneOf",
    "deprecated",
    "nullable",
];

/// Dropped: schema metadata, editor hints and vendor extensions (`x-...`).
fn ignored(key: &str) -> bool {
    matches!(key, "$schema" | "$comment" | "$id" | "title" | "markdownDescription" | "strict") || key.starts_with("x-")
}

/// Keys are always strings: saying so adds nothing.
fn string_keys(key: &str, v: &Value) -> bool {
    key == "propertyNames" && v.as_object().is_some_and(|m| m.len() == 1 && m.get("type") == Some(&Value::from("string")))
}

fn known(schema: &Map<String, Value>) -> bool {
    schema.iter().all(|(k, v)| ignored(k) || KNOWN.contains(&k.as_str()) || string_keys(k, v))
}

/// `$ref` anywhere: the definitions it points to are part of the schema, keep it whole.
fn has_ref(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("$ref") || m.values().any(has_ref),
        Value::Array(a) => a.iter().any(has_ref),
        _ => false,
    }
}

struct Writer {
    out: String,
    desc_max: Option<usize>,
}

impl Writer {
    fn text(&self, s: &str) -> String {
        let s = s.trim();
        self.desc_max.map_or_else(|| s.to_string(), |n| ellipsize(s, n))
    }

    /// `s` at `indent`, its later lines at `more` (blank lines stay empty).
    fn lines(&mut self, indent: usize, more: usize, s: &str) {
        for (i, l) in s.lines().enumerate() {
            self.out.push('\n');
            if !l.trim().is_empty() {
                self.out.push_str(&" ".repeat(if i == 0 { indent } else { more }));
                self.out.push_str(l.trim_end());
            }
        }
    }

    /// `s` at `indent`, its later lines indented two more.
    fn line(&mut self, indent: usize, s: &str) {
        self.lines(indent, indent + 2, s);
    }

    /// The properties of an object schema, one line each, nested ones indented.
    fn properties(&mut self, schema: &Map<String, Value>, indent: usize) {
        let required: Vec<&str> =
            schema.get("required").and_then(Value::as_array).map_or(vec![], |r| r.iter().filter_map(Value::as_str).collect());
        let Some(props) = schema.get("properties").and_then(Value::as_object) else { return };
        for (name, p) in props {
            self.property(name, p, required.contains(&name.as_str()), indent);
        }
    }

    fn property(&mut self, name: &str, p: &Value, required: bool, indent: usize) {
        let Some(m) = p.as_object() else {
            // `true` (anything) or a malformed entry
            self.line(indent, &format!("- {name}{}", if required { " (required)" } else { "" }));
            return;
        };
        let desc = m.get("description").and_then(Value::as_str).map(|d| self.text(d)).filter(|d| !d.is_empty());
        let shape = shape(m);
        let mut head = vec![];
        if let Some(s) = &shape {
            head.push(s.kind.clone());
        }
        if required {
            head.push("required".into());
        }
        if shape.is_some() {
            head.extend(constraints(m));
        }
        let mut line = format!("- {name}");
        if !head.is_empty() {
            line.push_str(&format!(" ({})", head.join(", ")));
        }
        if let Some(d) = &desc {
            line.push_str(": ");
            line.push_str(d);
        }
        self.line(indent, &line);
        match shape {
            None => {
                // what the compact form cannot say, as JSON Schema
                let mut rest = m.clone();
                rest.remove("description");
                rest.retain(|k, v| !ignored(k) && !string_keys(k, v));
                self.line(indent + 2, &format!("schema: {}", Value::Object(rest)));
            }
            Some(s) => {
                if let Some(values) = m.get("enum").and_then(Value::as_array)
                    && let Some(notes) = m.get("enumDescriptions").and_then(Value::as_array)
                {
                    for (v, n) in values.iter().zip(notes) {
                        if let Some(n) = n.as_str().filter(|n| !n.trim().is_empty()) {
                            let n = self.text(n);
                            self.line(indent + 2, &format!("{v}: {n}"));
                        }
                    }
                }
                for n in &s.notes {
                    let mut parts = n.constraints.clone();
                    parts.extend(n.description.map(|d| self.text(d)));
                    self.line(indent + 2, &format!("{}: {}", n.of, parts.join("; ")));
                }
                if let Some(child) = s.children {
                    self.properties(child, indent + 2);
                }
            }
        }
    }
}

/// How a schema reads in one line, the object whose properties go below it, and what its items or values say
/// (`each item: max length 10; a file path`).
struct Shape<'a> {
    kind: String,
    children: Option<&'a Map<String, Value>>,
    notes: Vec<Note<'a>>,
}

struct Note<'a> {
    of: &'static str,
    constraints: Vec<String>,
    description: Option<&'a str>,
}

impl<'a> Note<'a> {
    /// What an array's items or an object's values add to their type; None when nothing.
    fn of(of: &'static str, m: &'a Map<String, Value>) -> Option<Note<'a>> {
        let description = m.get("description").and_then(Value::as_str).filter(|d| !d.trim().is_empty());
        let constraints = constraints(m);
        (description.is_some() || !constraints.is_empty()).then_some(Note { of, constraints, description })
    }
}

/// None when the compact form cannot say all of it.
fn shape(m: &Map<String, Value>) -> Option<Shape<'_>> {
    if !known(m) {
        return None;
    }
    if let Some(c) = m.get("const") {
        return Some(Shape { kind: format!("always {c}"), children: None, notes: vec![] });
    }
    if let Some(values) = m.get("enum").and_then(Value::as_array) {
        let kind = values.iter().map(Value::to_string).collect::<Vec<_>>().join(" | ");
        return Some(Shape { kind: if values.is_empty() { "enum".into() } else { kind }, children: None, notes: vec![] });
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = m.get(key).and_then(Value::as_array) {
            if m.contains_key("type") || m.contains_key("properties") {
                return None;
            }
            let mut kinds = vec![];
            for b in branches {
                let s = shape(b.as_object()?)?;
                if s.children.is_some() || !s.notes.is_empty() || Note::of("", b.as_object()?).is_some() {
                    return None;
                }
                kinds.push(s.kind);
            }
            return Some(Shape { kind: kinds.join(" | "), children: None, notes: vec![] });
        }
    }
    let types: Vec<&str> = match m.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        Some(_) => return None,
        None if m.contains_key("properties") => vec!["object"],
        None if m.contains_key("items") => vec!["array"],
        None => vec![],
    };
    let mut children = None;
    let mut notes = vec![];
    let mut kinds = vec![];
    for t in &types {
        let kind = match *t {
            "object" => {
                if m.contains_key("properties") {
                    if children.is_some() {
                        return None;
                    }
                    children = Some(m);
                    "object".to_string()
                } else {
                    match m.get("additionalProperties") {
                        Some(Value::Object(v)) if v.is_empty() => "object".to_string(),
                        Some(Value::Object(v)) => {
                            let s = shape(v)?;
                            if s.children.is_some() || !s.notes.is_empty() {
                                return None;
                            }
                            notes.extend(Note::of("each value", v));
                            format!("object of {}", s.kind)
                        }
                        _ => "object".to_string(),
                    }
                }
            }
            "array" => match m.get("items") {
                None => "array".to_string(),
                Some(Value::Object(items)) => {
                    let s = shape(items)?;
                    if !s.notes.is_empty() {
                        return None;
                    }
                    notes.extend(Note::of("each item", items));
                    if let Some(c) = s.children {
                        if children.is_some() {
                            return None;
                        }
                        children = Some(c);
                    }
                    if s.kind.contains(' ') { format!("({})[]", s.kind) } else { format!("{}[]", s.kind) }
                }
                Some(_) => return None,
            },
            other => other.to_string(),
        };
        kinds.push(kind);
    }
    if m.contains_key("properties") && children.is_none() {
        return None;
    }
    if m.get("additionalProperties").is_some_and(Value::is_object) && m.contains_key("properties") {
        return None;
    }
    Some(Shape { kind: if kinds.is_empty() { "any".into() } else { kinds.join(" | ") }, children, notes })
}

/// Bounds, formats and defaults, as short phrases.
fn constraints(m: &Map<String, Value>) -> Vec<String> {
    let mut out = vec![];
    let num = |k: &str| m.get(k).filter(|v| v.is_number()).map(Value::to_string);
    for (key, label) in [
        ("minimum", "min"),
        ("maximum", "max"),
        ("exclusiveMinimum", "greater than"),
        ("exclusiveMaximum", "less than"),
        ("minLength", "min length"),
        ("maxLength", "max length"),
        ("minItems", "min items"),
        ("maxItems", "max items"),
        ("multipleOf", "multiple of"),
    ] {
        if let Some(v) = num(key) {
            out.push(format!("{label} {v}"));
        }
    }
    if m.get("uniqueItems") == Some(&Value::Bool(true)) {
        out.push("unique items".into());
    }
    if let Some(p) = m.get("pattern").and_then(Value::as_str) {
        out.push(format!("pattern {p}"));
    }
    if let Some(f) = m.get("format").and_then(Value::as_str) {
        out.push(format!("format {f}"));
    }
    if m.get("nullable") == Some(&Value::Bool(true)) {
        out.push("may be null".into());
    }
    if let Some(d) = m.get("default") {
        out.push(format!("default {d}"));
    }
    if m.get("deprecated") == Some(&Value::Bool(true)) {
        out.push("deprecated".into());
    }
    out
}

/// One tool: `### name`, its description indented (its own markdown headings must not read as another tool), then
/// its parameters (or the JSON Schema, when the compact form cannot say it). `desc_max` > 0 cuts descriptions.
pub fn compact_tool(t: &ToolSpec, desc_max: i64) -> String {
    let desc_max = usize::try_from(desc_max).ok().filter(|n| *n > 0);
    let mut w = Writer { out: format!("### {}", t.name), desc_max };
    let desc = w.text(&t.description);
    if !desc.is_empty() {
        w.lines(2, 2, &desc);
    }
    let params = t.parameters.as_object();
    let empty = params.is_none_or(|p| p.get("properties").and_then(Value::as_object).is_none_or(Map::is_empty));
    let compact = params.is_some_and(|p| {
        known(p) && !has_ref(&t.parameters) && shape(p).is_some_and(|s| s.kind == "object" && (s.children.is_some() || empty))
    });
    if empty && compact {
        w.line(0, "Parameters: none");
    } else if let (true, Some(p)) = (compact, params) {
        w.line(0, "Parameters:");
        w.properties(p, 0);
    } else {
        let mut schema = t.parameters.clone();
        if let Some(m) = schema.as_object_mut() {
            m.retain(|k, v| !ignored(k) && !string_keys(k, v));
        }
        w.line(0, &format!("Parameters (JSON Schema): {schema}"));
    }
    w.out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(params: Value) -> ToolSpec {
        ToolSpec::new("t", "Does things.", Some(params), false)
    }

    #[test]
    fn simple_tools_read_as_lines() {
        let t = tool(json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "File to read.\nAbsolute."},
                "start": {"type": "integer", "minimum": 1, "default": 1},
                "mode": {"type": "string", "enum": ["sync", "async"], "enumDescriptions": ["Wait.", "Do not wait."]},
                "tags": {"type": "array", "items": {"type": "string", "maxLength": 9, "description": "A tag."}, "maxItems": 5},
                "edits": {"type": "array", "items": {"type": "object", "properties": {"old": {"type": "string"}, "new": {"type": "string"}}, "required": ["old"]}},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "propertyNames": {"type": "string"}},
                "extra": {"type": "object", "additionalProperties": {}},
                "owner": {"type": ["string", "null"], "x-mcp-header": "owner"}
            },
            "required": ["path", "edits"]
        }));
        assert_eq!(
            compact_tool(&t, 0),
            "### t\n  Does things.\nParameters:\n\
             - path (string, required): File to read.\n  Absolute.\n\
             - start (integer, min 1, default 1)\n\
             - mode (\"sync\" | \"async\")\n  \"sync\": Wait.\n  \"async\": Do not wait.\n\
             - tags (string[], max items 5)\n  each item: max length 9; A tag.\n\
             - edits (object[], required)\n  - old (string, required)\n  - new (string)\n\
             - env (object of string)\n\
             - extra (object)\n\
             - owner (string | null)"
        );
    }

    #[test]
    fn what_the_compact_form_cannot_say_stays_json_schema() {
        let t = tool(json!({
            "type": "object",
            "properties": {
                "q": {"type": "string"},
                "filter": {"description": "Pick one.", "anyOf": [{"type": "object", "properties": {"a": {"type": "string"}}}, {"type": "string"}]},
                "meta": {"type": "object", "propertyNames": {"pattern": "^x"}}
            }
        }));
        let out = compact_tool(&t, 0);
        assert!(out.contains("- q (string)\n"), "{out}");
        assert!(out.contains("- filter: Pick one.\n  schema: {\"anyOf\":[{\"type\":\"object\",\"properties\":{\"a\":{\"type\":\"string\"}}},{\"type\":\"string\"}]}"), "{out}");
        assert!(out.contains("- meta\n  schema: {\"type\":\"object\",\"propertyNames\":{\"pattern\":\"^x\"}}"), "{out}");
        let r = tool(json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}}, "$defs": {"A": {"type": "string"}}}));
        assert_eq!(
            compact_tool(&r, 0),
            "### t\n  Does things.\nParameters (JSON Schema): {\"type\":\"object\",\"properties\":{\"a\":{\"$ref\":\"#/$defs/A\"}},\"$defs\":{\"A\":{\"type\":\"string\"}}}"
        );
    }

    #[test]
    fn tools_without_parameters_say_so() {
        assert_eq!(compact_tool(&tool(json!({"type": "object", "properties": {}})), 0), "### t\n  Does things.\nParameters: none");
        assert_eq!(compact_tool(&ToolSpec::new("t", "", None, false), 0), "### t\nParameters: none");
    }

    #[test]
    fn descriptions_are_cut_when_asked() {
        let t = tool(json!({"type": "object", "properties": {"a": {"type": "string", "description": "abcdefghij"}}}));
        assert_eq!(compact_tool(&t, 4), "### t\n  Does…\nParameters:\n- a (string): abcd…");
    }

    #[test]
    fn a_description_with_headings_stays_inside_its_tool() {
        let t = ToolSpec::new("Bash", "Runs a command.\n\n# Instructions\nQuote paths.", None, false);
        assert_eq!(compact_tool(&t, 0), "### Bash\n  Runs a command.\n\n  # Instructions\n  Quote paths.\nParameters: none");
    }
}
