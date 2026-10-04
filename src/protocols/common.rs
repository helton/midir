//! What the protocol adapters share: decoding a request body straight from its bytes (errors name the offending
//! field), OpenAI-style content (a string or a list of parts) as text, tool arguments, parameters accepted without
//! effect, `max_tokens` validation, and the request facts the HTTP layer and telemetry need.
//!
//! Types that accept several JSON shapes (a string or a list, a string or an object) have hand-written visitors:
//! serde's `untagged` and `flatten` buffer the input first, which costs a copy of every message and loses the exact
//! numbers of `arbitrary_precision`.

use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{self, DeserializeOwned, Deserializer, IgnoredAny, MapAccess, SeqAccess, Unexpected, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use crate::errors::ClientError;
use crate::json;

/// Content kinds a text-only backend cannot receive: they become a placeholder (CAVEAT).
pub const MEDIA_TYPES: [&str; 8] = ["image_url", "input_image", "image", "input_audio", "audio", "file", "input_file", "document"];

/// The key serde_json uses for a number it hands over as a map (`arbitrary_precision`): a number where an object is
/// expected must be refused as a number, not read as an object with that key.
const NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// What the HTTP layer and telemetry take from a request besides its canonical form.
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    /// the requested model ("" when absent)
    pub model: String,
    pub stream: bool,
    /// a session id carried in the body: Claude Code's `metadata.user_id`, Responses' `prompt_cache_key` or `user`
    pub session: Option<String>,
    pub previous_response_id: Option<String>,
    /// chat streaming: end with a usage chunk (`stream_options.include_usage`)
    pub include_usage: bool,
}

// ---------------------------------------------------------------------------------------------------------------------
// decoding
// ---------------------------------------------------------------------------------------------------------------------

/// The request body as a protocol's typed request, decoded from the bytes in one pass. Not JSON, or JSON that is not
/// an object, is a 400 that says so; a field of the wrong type is a 400 that names it.
pub fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, ClientError> {
    if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(match serde_json::from_slice::<IgnoredAny>(body) {
            Ok(_) => ClientError::new("request body must be a JSON object", "invalid_json"),
            Err(e) => not_json(&e),
        });
    }
    let mut de = serde_json::Deserializer::from_slice(body);
    let value: T = serde_path_to_error::deserialize(&mut de)
        .map_err(|e| if e.inner().is_data() { invalid(&e.path().to_string(), e.inner()) } else { not_json(e.inner()) })?;
    de.end().map_err(|e| not_json(&e))?;
    Ok(value)
}

/// A field kept as raw JSON (echoed as is) decoded on its own; errors name the field.
pub fn field<T: DeserializeOwned>(raw: Option<&RawValue>, name: &str) -> Result<Option<T>, ClientError> {
    let Some(raw) = raw else { return Ok(None) };
    let mut de = serde_json::Deserializer::from_str(raw.get());
    serde_path_to_error::deserialize(&mut de).map(Some).map_err(|e| {
        let path = e.path().to_string();
        let at = match path.as_str() {
            "." => name.to_string(),
            p if p.starts_with('[') => format!("{name}{p}"),
            p => format!("{name}.{p}"),
        };
        invalid(&at, e.inner())
    })
}

fn not_json(e: &serde_json::Error) -> ClientError {
    ClientError::new(format!("request body is not valid JSON ({e})"), "invalid_json")
}

fn invalid(path: &str, e: &serde_json::Error) -> ClientError {
    let at = if path == "." { String::new() } else { format!("{path}: ") };
    // serde_json appends the position; for a field named by its path, the position only adds noise
    let msg = e.to_string();
    let msg = msg.rsplit_once(" at line ").map_or(msg.as_str(), |(m, _)| m);
    ClientError::new(format!("invalid request: {at}{msg}"), "invalid_request")
}

/// `"stream": true` and friends: only a JSON `true` turns a flag on (anything else is off, as before).
pub fn is_true(raw: Option<&RawValue>) -> bool {
    raw.is_some_and(|r| r.get().trim() == "true")
}

/// Parameters accepted without effect: present and not null, false, empty or zero-length.
pub fn ignored_params(fields: &[(&str, Option<&RawValue>)]) -> Vec<String> {
    fields.iter().filter(|(_, raw)| raw.is_some_and(json::is_meaningful)).map(|(name, _)| name.to_string()).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// shapes shared by the protocols
// ---------------------------------------------------------------------------------------------------------------------

/// Deserialize a value that is a string or a list of `T`.
fn text_or_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D, what: &'static str) -> Result<Result<String, Vec<T>>, D::Error> {
    struct V<T>(&'static str, PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Result<String, Vec<T>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str(self.0)
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok(Ok(s.to_string()))
        }
        fn visit_string<E: de::Error>(self, s: String) -> Result<Self::Value, E> {
            Ok(Ok(s))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element()? {
                out.push(item);
            }
            Ok(Err(out))
        }
    }
    d.deserialize_any(V(what, PhantomData))
}

/// Deserialize a value that is a string or an object (decoded as `T`).
fn text_or_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D, what: &'static str) -> Result<Result<String, T>, D::Error> {
    struct V<T>(&'static str, PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Result<String, T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str(self.0)
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok(Ok(s.to_string()))
        }
        fn visit_string<E: de::Error>(self, s: String) -> Result<Self::Value, E> {
            Ok(Ok(s))
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map)).map(Err)
        }
    }
    d.deserialize_any(V(what, PhantomData))
}

/// A content object (an OpenAI content part, an Anthropic block) read key by key: the keys a type knows are decoded
/// by `known` (which returns false for anything else), the rest are kept in order as JSON values.
pub fn object_fields<'de, A: MapAccess<'de>>(
    mut map: A,
    what: &'static str,
    mut known: impl FnMut(&str, &mut A) -> Result<bool, A::Error>,
) -> Result<Map<String, Value>, A::Error> {
    let mut extra = Map::new();
    while let Some(key) = map.next_key::<String>()? {
        if key == NUMBER_TOKEN {
            return Err(de::Error::invalid_type(Unexpected::Other("number"), &Expecting(what)));
        }
        if !known(&key, &mut map)? {
            let value: Value = map.next_value()?;
            extra.insert(key, value);
        }
    }
    Ok(extra)
}

/// What was expected, for errors raised outside a visitor.
struct Expecting(&'static str);

impl de::Expected for Expecting {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// OpenAI content: a string, or a list of parts (strings or typed objects).
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match text_or_list(d, "a string or a list of content parts")? {
            Ok(s) => Content::Text(s),
            Err(parts) => Content::Parts(parts),
        })
    }
}

#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    Block(PartBlock),
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match text_or_object(d, "a string or a content part object")? {
            Ok(s) => Part::Text(s),
            Err(b) => Part::Block(b),
        })
    }
}

/// A typed content part: text, refusal, media, or anything else (kept to be shown as JSON).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PartBlock {
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl<'de> Deserialize<'de> for PartBlock {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = PartBlock;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a content part object")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<PartBlock, A::Error> {
                let mut b = PartBlock::default();
                let (mut kind, mut text, mut refusal) = (None, None, None);
                b.extra = object_fields(map, "a content part object", |key, map| {
                    match key {
                        "type" => kind = map.next_value::<Option<String>>()?,
                        "text" => text = map.next_value()?,
                        "refusal" => refusal = map.next_value()?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                (b.kind, b.text, b.refusal) = (kind.unwrap_or_default(), text, refusal);
                Ok(b)
            }
        }
        d.deserialize_map(V)
    }
}

pub fn placeholder(kind: &str, place: &str) -> String {
    tracing::warn!("{place}: '{kind}' content replaced by a placeholder (the backend is text-only)");
    format!("[{kind} omitted: this model only receives text]")
}

impl PartBlock {
    /// Text parts as they are, refusals as their text, media as a placeholder, anything else as its JSON.
    pub fn into_text(self, place: &str) -> String {
        match self.kind.as_str() {
            "text" | "input_text" | "output_text" => self.text.unwrap_or_default(),
            "refusal" => self.refusal.unwrap_or_default(),
            k if MEDIA_TYPES.contains(&k) => placeholder(k, place),
            _ => json::readable(&self),
        }
    }
}

impl Content {
    /// The content as one text; parts are joined by newlines.
    pub fn into_text(self, place: &str) -> String {
        match self {
            Content::Text(s) => s,
            Content::Parts(parts) => parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(s) => s,
                    Part::Block(b) => b.into_text(place),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

pub fn text_of(content: Option<Content>, place: &str) -> String {
    content.map_or_else(String::new, |c| c.into_text(place))
}

/// One stop sequence or several.
#[derive(Debug, Clone)]
pub struct Stop(pub Vec<String>);

impl<'de> Deserialize<'de> for Stop {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Stop(match text_or_list(d, "a string or a list of strings")? {
            Ok(s) => vec![s],
            Err(v) => v,
        }))
    }
}

pub fn stops(stop: Option<Stop>) -> Vec<String> {
    stop.map(|s| s.0).unwrap_or_default()
}

/// A value that is a string (a mode: "auto", "none", "required") or an object naming something (a tool choice).
#[derive(Debug, Clone)]
pub enum ModeOr<T> {
    Mode(String),
    Object(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ModeOr<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match text_or_object(d, "\"auto\", \"none\", \"required\" or an object naming a tool")? {
            Ok(s) => ModeOr::Mode(s),
            Err(o) => ModeOr::Object(o),
        })
    }
}

/// Tool-call arguments as a JSON value: a JSON string is decoded (an empty one is `{}`); a string that is not JSON
/// stays a string (CAVEAT: invalid JSON in a client's history is passed on as text).
pub fn parse_arguments(raw: Option<Value>) -> Value {
    match raw {
        None | Some(Value::Null) => json!({}),
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        Some(other) => other,
    }
}

/// Arguments as the JSON text clients expect in `arguments` fields.
pub fn arguments_text(a: &Value) -> String {
    match a {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A JSON Schema for tool parameters: the client's object, else an empty object schema.
pub fn tool_params(p: Option<Value>) -> Option<Value> {
    p.filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
}

/// `max_tokens` and its equivalents must be positive.
pub fn positive(name: &str, v: Option<i64>) -> Result<Option<i64>, ClientError> {
    match v {
        Some(n) if n <= 0 => Err(ClientError::new(format!("{name} must be a positive integer, got {n}"), "invalid_request")),
        other => Ok(other),
    }
}

/// A named tool choice must name a declared tool (as OpenAI and Anthropic require).
pub fn check_named_choice(name: &str, tools: &[crate::canonical::ToolSpec]) -> Result<(), ClientError> {
    if tools.iter().any(|t| t.name == name) {
        return Ok(());
    }
    Err(ClientError::new(format!("tool_choice names the tool '{name}', which is not in tools"), "invalid_tool_choice"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn any_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            "[a-z ]{0,8}".prop_map(Value::String),
            prop::sample::select(vec![
                "user",
                "assistant",
                "tool",
                "system",
                "text",
                "tool_use",
                "tool_result",
                "function",
                "message",
                "function_call",
                "function_call_output",
                "item_reference"
            ])
            .prop_map(|s| json!(s)),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
                prop::collection::vec(
                    (
                        prop::sample::select(vec![
                            "role",
                            "content",
                            "type",
                            "text",
                            "tools",
                            "tool_calls",
                            "function",
                            "name",
                            "arguments",
                            "input",
                            "messages",
                            "system",
                            "tool_choice",
                            "id",
                            "call_id",
                            "output",
                            "max_tokens",
                            "stop",
                            "x"
                        ]),
                        inner
                    ),
                    0..5
                )
                .prop_map(|kv| Value::Object(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn decoders_answer_any_json_without_panicking(messages in any_json(), extra in any_json()) {
            let body = json!({"model": "m", "messages": messages, "input": extra.clone(), "tools": extra, "max_tokens": 5}).to_string();
            let _ = crate::protocols::chat_completions::to_canonical(body.as_bytes());
            let _ = crate::protocols::messages::to_canonical(body.as_bytes());
            let _ = crate::protocols::responses::decode_request(body.as_bytes());
        }
    }

    #[derive(Deserialize, Debug)]
    struct Probe {
        content: Option<Content>,
        stop: Option<Stop>,
        choice: Option<ModeOr<Map<String, Value>>>,
        args: Option<Value>,
    }

    #[test]
    fn shapes_and_errors() {
        let p: Probe =
            decode(br#"{"content": ["a", {"type": "text", "text": "b"}, {"type": "x", "n": 1.50}], "stop": "END", "choice": "auto"}"#)
                .unwrap();
        assert_eq!(p.content.unwrap().into_text("t"), "a\nb\n{\"type\": \"x\", \"n\": 1.50}");
        assert_eq!(p.stop.unwrap().0, vec!["END"]);
        assert!(matches!(p.choice, Some(ModeOr::Mode(m)) if m == "auto"));
        let e = decode::<Probe>(br#"{"content": [{"type": "text", "text": 5}]}"#).unwrap_err();
        assert_eq!(e.message, "invalid request: content[0].text: invalid type: integer `5`, expected a string");
        let e = decode::<Probe>(br#"{"content": [1.5]}"#).unwrap_err();
        assert!(e.message.starts_with("invalid request: content[0]: invalid type: number"), "{}", e.message);
        assert_eq!(decode::<Probe>(b"[1]").unwrap_err().message, "request body must be a JSON object");
        assert!(decode::<Probe>(b"{nope").unwrap_err().message.starts_with("request body is not valid JSON"));
        assert!(decode::<Probe>(br#"{"stop": 5}"#).unwrap_err().message.contains("a string or a list of strings"));
    }

    #[test]
    fn raw_field_errors_name_the_path() {
        #[derive(Deserialize, Debug)]
        struct Tool {
            #[allow(dead_code)]
            name: Option<String>,
        }
        let raw: Box<RawValue> = serde_json::from_str(r#"[{"name": 5}]"#).unwrap();
        let e = field::<Vec<Tool>>(Some(&raw), "tools").unwrap_err();
        assert!(e.message.starts_with("invalid request: tools[0].name: invalid type"), "{}", e.message);
    }

    #[test]
    fn numbers_keep_their_digits() {
        let p: Probe = decode(br#"{"args": {"big": 123456789012345678901234567890, "f": 0.1, "e": 1e400}}"#).unwrap();
        assert_eq!(p.args.unwrap().to_string(), r#"{"big":123456789012345678901234567890,"f":0.1,"e":1e+400}"#);
    }

    #[test]
    fn meaningful_parameters() {
        let raw = |s: &str| serde_json::from_str::<Box<RawValue>>(s).unwrap();
        let (t, f, e, o, z) = (raw("0.7"), raw("false"), raw("[ ]"), raw("{\"a\": 1}"), raw("0"));
        let found = ignored_params(&[("t", Some(&t)), ("f", Some(&f)), ("e", Some(&e)), ("o", Some(&o)), ("z", Some(&z)), ("n", None)]);
        assert_eq!(found, vec!["t", "o", "z"]);
    }
}
