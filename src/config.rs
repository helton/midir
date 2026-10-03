//! Configuration: config/midir.toml (or MIDIR_CONFIG) plus the environment. `${NAME}` references are
//! expanded from the environment; an environment variable with the same meaning always wins over the file. The
//! pre-0.0.1 layout (`default`, `[stackspot]`, `[limits]`, `[[agents]]`) is still read, with a warning.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::py::obj;
use crate::py::text::{self, str_of, truthy};

pub const DEFAULT_MODEL_NAME: &str = "default";
const FALSE: [&str; 4] = ["0", "false", "no", "off"];
const LOG: &str = "midir.config";

#[derive(Debug, Clone)]
pub struct LimitSettings {
    pub max_concurrent: i64,
    pub requests_per_minute: i64,
    pub queue_timeout_s: f64,
    pub cooldown_on_429_s: f64,
}

#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub name: String,
    pub type_: String,
    pub options: Map<String, Value>,
    pub limits: LimitSettings,
}

#[derive(Debug)]
pub struct ModelSpec {
    pub name: String,
    pub backend: String,
    pub target: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub match_re: Option<fancy_regex::Regex>,
    pub match_pattern: Option<String>,
    pub max_prompt_chars: Option<Value>,
    pub tail_reminder: Option<Value>,
    pub tool_desc_max: Option<Value>,
}

impl ModelSpec {
    fn simple(name: &str, backend: &str, target: &str, description: &str) -> Self {
        ModelSpec {
            name: name.into(),
            backend: backend.into(),
            target: target.into(),
            description: description.into(),
            aliases: vec![],
            match_re: None,
            match_pattern: None,
            max_prompt_chars: None,
            tail_reminder: None,
            tool_desc_max: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub port: i64,
    pub max_prompt_chars: i64,
    pub tail_reminder: bool,
    pub tool_desc_max: i64,
    pub responses_dir: Option<PathBuf>,
    pub responses_retention_days: f64,
    pub responses_max_mb: f64,
    pub keepalive_s: f64,
    pub retry_backoff_s: f64,
}

#[derive(Debug, Clone)]
pub struct TelemetrySettings {
    pub otlp_endpoint: String,
    pub service_name: String,
}

/// Invalid configuration: the process stops with this message.
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

type R<T> = Result<T, ConfigError>;

pub struct Config {
    pub env: IndexMap<String, String>,
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub source: String,
    pub server: ServerSettings,
    pub telemetry: TelemetrySettings,
    pub backends: IndexMap<String, BackendSettings>,
    pub models: Vec<Arc<ModelSpec>>,
    pub default: Arc<ModelSpec>,
}

/// The process environment, in its own order (like `dict(os.environ)`).
pub fn environment() -> IndexMap<String, String> {
    std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect()
}

pub fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => crate::py::json::float(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect()),
    }
}

fn int_cfg(v: &Value) -> R<i64> {
    text::int_of(v).ok_or_else(|| ConfigError(format!("invalid literal for int() with base 10: {}", text::repr(v))))
}

fn float_cfg(v: &Value) -> R<f64> {
    text::float_of(v).ok_or_else(|| ConfigError(format!("could not convert string to float: {}", text::repr(v))))
}

fn section(v: Option<&Value>) -> R<Option<&Map<String, Value>>> {
    match v {
        None => Ok(None),
        Some(x) if !truthy(x) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        Some(other) => Err(ConfigError(format!("AttributeError: '{}' object has no attribute 'get'", text::type_name(other)))),
    }
}

impl Config {
    pub fn load(env: IndexMap<String, String>, root: PathBuf) -> R<Config> {
        let config_file = match env.get("MIDIR_CONFIG").filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => root.join("config").join("midir.toml"),
        };
        Self::load_file(env, config_file, root)
    }

    pub fn load_file(env: IndexMap<String, String>, config_file: PathBuf, root: PathBuf) -> R<Config> {
        let is_file = config_file.is_file();
        let source = if is_file { config_file.display().to_string() } else { "env".into() };
        let mut data = Map::new();
        if is_file {
            let raw =
                std::fs::read_to_string(&config_file).map_err(|e| ConfigError(format!("{}: cannot read: {e}", config_file.display())))?;
            let parsed: toml::Table = raw
                .parse()
                .map_err(|e: toml::de::Error| ConfigError(format!("{}: cannot read: {}", config_file.display(), e.message())))?;
            data = match toml_to_json(toml::Value::Table(parsed)) {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            data = upgrade_legacy(&config_file, data);
        }
        let mut cfg = Config {
            env,
            root,
            config_file,
            source,
            server: ServerSettings {
                port: 18880,
                max_prompt_chars: 1_000_000,
                tail_reminder: true,
                tool_desc_max: 0,
                responses_dir: None,
                responses_retention_days: 30.0,
                responses_max_mb: 500.0,
                keepalive_s: 15.0,
                retry_backoff_s: 1.0,
            },
            telemetry: TelemetrySettings { otlp_endpoint: String::new(), service_name: "midir".into() },
            backends: IndexMap::new(),
            models: vec![],
            default: Arc::new(ModelSpec::simple("", "", "", "")),
        };
        cfg.server = cfg.server_settings(section(data.get("server"))?)?;
        let tel = section(data.get("telemetry"))?;
        let endpoint = cfg.get(tel, "otlp_endpoint", "OTEL_EXPORTER_OTLP_ENDPOINT", json!(""));
        let endpoint = if truthy(&endpoint) { str_of(&endpoint) } else { String::new() };
        cfg.telemetry = TelemetrySettings {
            otlp_endpoint: text::strip(&endpoint).to_string(),
            service_name: str_of(&cfg.get(tel, "service_name", "OTEL_SERVICE_NAME", json!("midir"))),
        };
        let raw_backends = match data.get("backends") {
            Some(v) if truthy(v) => match v {
                Value::Object(m) => m.clone(),
                other => return Err(ConfigError(format!("AttributeError: '{}' object has no attribute 'items'", text::type_name(other)))),
            },
            _ => Map::new(),
        };
        cfg.backends = cfg.backend_settings(raw_backends)?;
        if truthy_opt(data.get("models")) {
            cfg.models_from_file(&data)?;
        } else {
            cfg.models_from_env()?;
        }
        Ok(cfg)
    }

    /// `${NAME}` references replaced by the environment (empty when unset).
    pub fn expand(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(expand_str(s, &self.env)),
            Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), self.expand(x))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.expand(x)).collect()),
            other => other.clone(),
        }
    }

    fn get(&self, section: Option<&Map<String, Value>>, key: &str, env_name: &str, default: Value) -> Value {
        if let Some(v) = self.env.get(env_name).filter(|v| !v.is_empty()) {
            return Value::String(v.clone());
        }
        match section.and_then(|s| s.get(key)) {
            None | Some(Value::Null) => default,
            Some(v) => self.expand(v),
        }
    }

    fn server_settings(&self, s: Option<&Map<String, Value>>) -> R<ServerSettings> {
        let rdir = self.get(s, "responses_dir", "MIDIR_RESPONSES_DIR", json!("data/gateway/responses"));
        let rdir = if truthy(&rdir) { str_of(&rdir) } else { String::new() };
        let rdir = text::strip(&rdir).to_string();
        let responses_dir = if rdir.is_empty() {
            None
        } else if Path::new(&rdir).is_absolute() {
            Some(PathBuf::from(&rdir))
        } else {
            Some(self.root.join(&rdir))
        };
        Ok(ServerSettings {
            port: int_cfg(&self.get(s, "port", "MIDIR_PORT", json!(18880)))?,
            max_prompt_chars: int_cfg(&self.get(s, "max_prompt_chars", "MIDIR_MAX_PROMPT_CHARS", json!(1_000_000)))?,
            tail_reminder: !FALSE
                .contains(&str_of(&self.get(s, "tail_reminder", "MIDIR_TAIL_REMINDER", json!(true))).to_lowercase().as_str()),
            tool_desc_max: int_cfg(&self.get(s, "tool_desc_max", "MIDIR_TOOL_DESC_MAX", json!(0)))?,
            responses_dir,
            responses_retention_days: float_cfg(&self.get(s, "responses_retention_days", "MIDIR_RESPONSES_RETENTION_DAYS", json!(30)))?,
            responses_max_mb: float_cfg(&self.get(s, "responses_max_mb", "MIDIR_RESPONSES_MAX_MB", json!(500)))?,
            keepalive_s: float_cfg(&self.get(s, "keepalive_s", "MIDIR_KEEPALIVE_S", json!(15)))?,
            retry_backoff_s: float_cfg(&self.get(s, "retry_backoff_s", "MIDIR_RETRY_BACKOFF_S", json!(1)))?,
        })
    }

    fn backend_settings(&self, raw: Map<String, Value>) -> R<IndexMap<String, BackendSettings>> {
        let raw = if raw.is_empty() {
            let mut m = Map::new();
            m.insert("stackspot".into(), json!({"type": "stackspot"}));
            m
        } else {
            raw
        };
        let mut out = IndexMap::new();
        for (name, b) in raw {
            let Value::Object(b) = b else {
                return Err(ConfigError(format!("{}: [backends.{name}] must be a table", self.config_file.display())));
            };
            let lim = section(b.get("limits"))?;
            let limits = LimitSettings {
                max_concurrent: int_cfg(&self.get(lim, "max_concurrent", "MIDIR_MAX_CONCURRENT", json!(8)))?,
                requests_per_minute: int_cfg(&self.get(lim, "requests_per_minute", "MIDIR_REQUESTS_PER_MINUTE", json!(90)))?,
                queue_timeout_s: float_cfg(&self.get(lim, "queue_timeout_s", "MIDIR_QUEUE_TIMEOUT", json!(600)))?,
                cooldown_on_429_s: float_cfg(&self.get(lim, "cooldown_on_429_s", "MIDIR_COOLDOWN_ON_429", json!(15)))?,
            };
            let options: Map<String, Value> = b
                .iter()
                .filter(|(k, _)| k.as_str() != "type" && k.as_str() != "limits")
                .map(|(k, v)| (k.clone(), self.expand(v)))
                .collect();
            let type_ = match b.get("type") {
                Some(t) if truthy(t) => str_of(t),
                _ => name.clone(),
            };
            out.insert(name.clone(), BackendSettings { name, type_, options, limits });
        }
        Ok(out)
    }

    fn models_from_file(&mut self, data: &Map<String, Value>) -> R<()> {
        let file = self.config_file.display().to_string();
        let items = obj::iter(&data["models"]).map_err(|e| ConfigError(format!("{}: {}", e.kind, e.msg)))?;
        let mut names: Vec<String> = vec![];
        for (i, m) in items.iter().enumerate() {
            let Value::Object(m) = m else {
                return Err(ConfigError(format!("AttributeError: '{}' object has no attribute 'get'", text::type_name(m))));
            };
            let name = text::strip(&str_of(m.get("name").unwrap_or(&json!("")))).to_lowercase();
            let where_ = format!("{file}: models[{i}] ({})", if name.is_empty() { "?" } else { &name });
            if name.is_empty() || names.contains(&name) {
                return Err(ConfigError(format!("{where_}: needs a unique 'name'")));
            }
            names.push(name.clone());
            let backend = match m.get("backend") {
                Some(b) if truthy(b) => str_of(b),
                _ if self.backends.len() == 1 => self.backends.keys().next().cloned().unwrap_or_default(),
                _ => String::new(),
            };
            let backend = text::strip(&backend).to_string();
            if !self.backends.contains_key(&backend) {
                let mut sorted: Vec<String> = self.backends.keys().cloned().collect();
                sorted.sort();
                return Err(ConfigError(format!(
                    "{where_}: 'backend' must be one of {} (got {})",
                    text::repr_list(&sorted),
                    text::repr_str(&backend)
                )));
            }
            let target = text::strip(&str_of(&self.expand(m.get("target").unwrap_or(&json!(""))))).to_string();
            if target.is_empty() {
                return Err(ConfigError(format!("{where_}: 'target' is empty (unset environment variable?)")));
            }
            let (match_re, match_pattern) = match m.get("match") {
                Some(p) if truthy(p) => {
                    let pat = str_of(p);
                    let re = fancy_regex::Regex::new(&format!("(?i){pat}"))
                        .map_err(|e| ConfigError(format!("{where_}: invalid 'match' regex: {e}")))?;
                    (Some(re), Some(pat))
                }
                _ => (None, None),
            };
            let aliases = match m.get("aliases") {
                Some(a) => obj::iter(a)
                    .map_err(|e| ConfigError(format!("{}: {}", e.kind, e.msg)))?
                    .iter()
                    .map(|x| str_of(x).to_lowercase())
                    .collect(),
                None => vec![],
            };
            self.models.push(Arc::new(ModelSpec {
                name,
                backend,
                target,
                description: m.get("description").map(str_of).unwrap_or_default(),
                aliases,
                match_re,
                match_pattern,
                max_prompt_chars: m.get("max_prompt_chars").cloned(),
                tail_reminder: m.get("tail_reminder").cloned(),
                tool_desc_max: m.get("tool_desc_max").cloned(),
            }));
        }
        let default_name = text::strip(&str_of(data.get("default_model").unwrap_or(&json!("")))).to_lowercase();
        if !default_name.is_empty() {
            match self.models.iter().find(|x| x.name == default_name) {
                Some(spec) => self.default = spec.clone(),
                None => {
                    return Err(ConfigError(format!(
                        "{file}: default_model = {} is not one of the configured models",
                        text::repr_str(&default_name)
                    )))
                }
            }
        } else {
            self.default = self.models[0].clone();
        }
        Ok(())
    }

    fn models_from_env(&mut self) -> R<()> {
        let backend = if self.backends.contains_key("stackspot") {
            "stackspot".to_string()
        } else {
            self.backends.keys().next().cloned().unwrap_or_default()
        };
        let mut models = vec![];
        for (k, v) in &self.env {
            if let Some(mid) = k.strip_prefix("STACKSPOT_").and_then(|r| r.strip_suffix("_AGENT_ID")) {
                if !mid.is_empty() && !mid.contains('\n') && mid != "DEFAULT" && !text::is_blank(v) {
                    models.push(Arc::new(ModelSpec::simple(&model_name(mid), &backend, text::strip(v), "")));
                }
            }
        }
        self.models = models;
        let default_id = text::strip(self.env.get("STACKSPOT_DEFAULT_AGENT_ID").map(String::as_str).unwrap_or("")).to_string();
        if !default_id.is_empty() {
            self.default =
                Arc::new(ModelSpec::simple(DEFAULT_MODEL_NAME, &backend, &default_id, "default agent (STACKSPOT_DEFAULT_AGENT_ID)"));
        } else if let Some(first) = self.models.first() {
            self.default = first.clone();
        } else {
            return Err(ConfigError(format!(
                "no models configured: create {} (see config/midir.example.toml) or set STACKSPOT_DEFAULT_AGENT_ID",
                self.config_file.display()
            )));
        }
        Ok(())
    }

    /// The model spec for a requested model name: exact name or alias > `match` regex in file order > exact after
    /// stripping a provider prefix > longest configured name contained in the requested one > default.
    pub fn resolve(&self, model: &str) -> Arc<ModelSpec> {
        let m = text::strip(model).to_lowercase();
        for s in &self.models {
            if m == s.name || s.aliases.contains(&m) {
                return s.clone();
            }
        }
        for s in &self.models {
            if let Some(re) = &s.match_re {
                if re.is_match(&m).unwrap_or(false) {
                    return s.clone();
                }
            }
        }
        let prefixes: Vec<String> =
            self.backends.keys().map(|b| regex::escape(&format!("{b}-"))).chain([regex::escape("midir-")]).collect();
        let bare = match Regex::new(&format!(r"^(?:[\w.-]+/)?(?:{})?", prefixes.join("|"))) {
            Ok(re) => re.replace(&m, "").into_owned(),
            Err(_) => m.clone(),
        };
        for s in &self.models {
            if bare == s.name || s.aliases.contains(&bare) {
                return s.clone();
            }
        }
        let mut best: Option<&Arc<ModelSpec>> = None;
        for s in &self.models {
            if m.contains(s.name.as_str()) && best.map_or(true, |b| text::len(&s.name) > text::len(&b.name)) {
                best = Some(s);
            }
        }
        best.cloned().unwrap_or_else(|| self.default.clone())
    }

    pub fn exposed_models(&self) -> Vec<Arc<ModelSpec>> {
        if self.models.is_empty() {
            vec![self.default.clone()]
        } else {
            self.models.clone()
        }
    }

    /// (max_prompt_chars, tail_reminder, tool_desc_max) for a model: its own values, else [server]'s.
    pub fn knobs(&self, spec: Option<&ModelSpec>) -> (i64, bool, i64) {
        let s = &self.server;
        let Some(spec) = spec else { return (s.max_prompt_chars, s.tail_reminder, s.tool_desc_max) };
        let max_chars = match &spec.max_prompt_chars {
            Some(v) if truthy(v) => text::int_of(v).unwrap_or(s.max_prompt_chars),
            _ => s.max_prompt_chars,
        };
        let tail = spec.tail_reminder.as_ref().map_or(s.tail_reminder, truthy);
        let desc = spec.tool_desc_max.as_ref().map_or(s.tool_desc_max, |v| text::int_of(v).unwrap_or(0));
        (max_chars, tail, desc)
    }
}

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map_or(false, truthy)
}

/// `${NAME}` (NAME = [A-Z0-9_]+) replaced by the environment.
pub fn expand_str(s: &str, env: &IndexMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let name_len = after.bytes().take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_').count();
        if name_len > 0 && after.as_bytes().get(name_len) == Some(&b'}') {
            out.push_str(env.get(&after[..name_len]).map(String::as_str).unwrap_or(""));
            rest = &after[name_len + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// GPT_5_1 -> gpt-5.1, GPT_4_1_MINI -> gpt-4.1-mini, O3_MINI -> o3-mini (digit_digit becomes a dot).
pub fn model_name(env_part: &str) -> String {
    let low: Vec<char> = env_part.to_lowercase().chars().collect();
    let mut out = String::with_capacity(low.len());
    for (i, &c) in low.iter().enumerate() {
        if c == '_' {
            let prev_digit = i > 0 && is_unicode_digit(low[i - 1]);
            let next_digit = i + 1 < low.len() && is_unicode_digit(low[i + 1]);
            out.push(if prev_digit && next_digit { '.' } else { '-' });
        } else {
            out.push(c);
        }
    }
    out
}

fn is_unicode_digit(c: char) -> bool {
    c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric())
}

/// The pre-0.0.1 layout, translated: `default` -> default_model, [stackspot] + [limits] -> [backends.stackspot],
/// [[agents]] (agent_id) -> [[models]] (target).
fn upgrade_legacy(file: &Path, data: Map<String, Value>) -> Map<String, Value> {
    if !(truthy_opt(data.get("agents")) || truthy_opt(data.get("stackspot")) || data.contains_key("default")) {
        return data;
    }
    crate::warn!(
        LOG,
        "{} uses the pre-0.0.1 layout ([stackspot], [[agents]], default); it still works, see config/midir.example.toml",
        file.display()
    );
    let mut out: Map<String, Value> = data
        .iter()
        .filter(|(k, _)| !["default", "stackspot", "limits", "agents"].contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(d) = data.get("default") {
        out.entry("default_model").or_insert_with(|| d.clone());
    }
    let mut backend = Map::new();
    backend.insert("type".into(), json!("stackspot"));
    if let Some(Value::Object(s)) = data.get("stackspot") {
        for (k, v) in s {
            backend.insert(k.clone(), v.clone());
        }
    }
    if let Some(l) = data.get("limits").filter(|l| truthy(l)) {
        backend.insert("limits".into(), l.clone());
    }
    let backends = out.entry("backends").or_insert_with(|| json!({}));
    if let Value::Object(b) = backends {
        b.entry("stackspot").or_insert(Value::Object(backend));
    }
    if let Some(Value::Array(agents)) = data.get("agents").filter(|a| truthy(a)) {
        let models: Vec<Value> = agents
            .iter()
            .map(|a| {
                let mut m = Map::new();
                if let Value::Object(a) = a {
                    for (k, v) in a {
                        if k != "agent_id" {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                    m.insert("backend".into(), json!("stackspot"));
                    m.insert("target".into(), a.get("agent_id").cloned().unwrap_or(json!("")));
                }
                Value::Object(m)
            })
            .collect();
        out.insert("models".into(), Value::Array(models));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(model_name("GPT_5_1"), "gpt-5.1");
        assert_eq!(model_name("GPT_4_1_MINI"), "gpt-4.1-mini");
        assert_eq!(model_name("O3_MINI"), "o3-mini");
    }
}
