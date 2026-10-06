//! Configuration: config/midir.toml (or MIDIR_CONFIG) plus the environment. `${NAME}` references are expanded from
//! the environment; an environment variable with the same meaning always wins over the file. The pre-0.0.1 layout
//! (`default`, `[stackspot]`, `[limits]`, `[[agents]]`) is still read, with a warning.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use crate::text::char_len;

pub const DEFAULT_MODEL_NAME: &str = "default";

#[derive(Debug, Clone)]
pub struct LimitSettings {
    pub max_concurrent: i64,
    pub requests_per_minute: i64,
    pub queue_timeout_s: f64,
    pub cooldown_on_429_s: f64,
    /// requests that may wait for a slot at once; beyond it a new one is a 429 at once (each waiter holds its body)
    pub max_waiting: i64,
}

#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub name: String,
    pub type_: String,
    /// The backend's own options, `${NAME}` already expanded.
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
    pub match_re: Option<Regex>,
    pub match_pattern: Option<String>,
    pub max_prompt_chars: Option<i64>,
    pub tail_reminder: Option<bool>,
    pub tool_desc_max: Option<i64>,
    pub tool_schema: Option<ToolSchema>,
    pub followups: Option<bool>,
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
            tool_schema: None,
            followups: None,
        }
    }
}

/// How tools are listed in the prompt: raw JSON Schema, one line per tool, or the compact form (a heading per tool and
/// a line per parameter), which is shorter for large tool sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSchema {
    #[default]
    Json,
    Compact,
}

impl FromStr for ToolSchema {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "json" => Ok(ToolSchema::Json),
            "compact" => Ok(ToolSchema::Compact),
            other => Err(format!("{other:?} is not json or compact")),
        }
    }
}

/// The settings a model can override (`[[models]]`), resolved for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knobs {
    pub max_prompt_chars: i64,
    pub tail_reminder: bool,
    pub tool_desc_max: i64,
    pub tool_schema: ToolSchema,
    pub followups: bool,
}

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub port: u16,
    pub max_prompt_chars: i64,
    pub tail_reminder: bool,
    pub tool_desc_max: i64,
    pub tool_schema: ToolSchema,
    /// the automatic follow-ups for replies that announce without acting, deny a tool's ability or forget an
    /// ordered commit (`MIDIR_FOLLOWUPS`; a model can override it)
    pub followups: bool,
    pub responses_dir: Option<PathBuf>,
    pub responses_retention_days: f64,
    pub responses_max_mb: f64,
    pub keepalive_s: f64,
    /// a request's headers must arrive within this, and its body may not stall longer between chunks
    pub read_timeout_s: f64,
    pub retry_backoff_s: f64,
    /// memory budget of the Responses cache (misses are rebuilt from disk)
    pub responses_memory_mb: f64,
    /// how long a stop waits for in-flight requests (streams) to finish
    pub shutdown_grace_s: f64,
    /// when set, every endpoint but /health and /ready requires it
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TelemetrySettings {
    pub otlp_endpoint: String,
    pub service_name: String,
    /// serve the metrics at /metrics (Prometheus text)
    pub prometheus: bool,
}

/// Invalid configuration: the process stops with this message.
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type R<T> = Result<T, ConfigError>;

pub struct Config {
    pub env: IndexMap<String, String>,
    pub config_file: PathBuf,
    /// the file the configuration came from, or "env"
    pub source: String,
    pub server: ServerSettings,
    pub telemetry: TelemetrySettings,
    pub backends: IndexMap<String, BackendSettings>,
    pub models: Vec<Arc<ModelSpec>>,
    pub default: Arc<ModelSpec>,
}

/// The process environment.
pub fn environment() -> IndexMap<String, String> {
    std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// the file, as written (numbers and booleans may also be strings, so `${NAME}` works everywhere)
// ---------------------------------------------------------------------------------------------------------------------

/// A number, or a string holding one (after `${NAME}` expansion). Read as a JSON value first: the number's own
/// digits are parsed, whatever its JSON type.
fn num<'de, D: Deserializer<'de>, T: FromStr>(d: D) -> Result<Option<T>, D::Error>
where
    T::Err: std::fmt::Display,
{
    let text = match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => return Err(de::Error::custom(format!("expected a number, got {other}"))),
    };
    text.parse().map(Some).map_err(|e| de::Error::custom(format!("{text:?} is not a valid number here ({e})")))
}

/// A word from a fixed set (`tool_schema = "compact"`).
fn word<'de, D: Deserializer<'de>, T: FromStr<Err = String>>(d: D) -> Result<Option<T>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => s.parse().map(Some).map_err(de::Error::custom),
    }
}

/// A boolean, or a string holding one (true/false, 1/0, yes/no, on/off).
fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(b)),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => parse_flag(&s).map(Some).ok_or_else(|| de::Error::custom(format!("{s:?} is not a boolean"))),
        Some(other) => Err(de::Error::custom(format!("expected a boolean, got {other}"))),
    }
}

fn parse_flag(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    default_model: Option<String>,
    server: ServerFile,
    telemetry: TelemetryFile,
    backends: IndexMap<String, BackendFile>,
    models: Vec<ModelFile>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerFile {
    #[serde(deserialize_with = "num")]
    port: Option<u16>,
    #[serde(deserialize_with = "num")]
    max_prompt_chars: Option<i64>,
    #[serde(deserialize_with = "flag")]
    tail_reminder: Option<bool>,
    #[serde(deserialize_with = "num")]
    tool_desc_max: Option<i64>,
    #[serde(deserialize_with = "word")]
    tool_schema: Option<ToolSchema>,
    #[serde(deserialize_with = "flag")]
    followups: Option<bool>,
    responses_dir: Option<String>,
    #[serde(deserialize_with = "num")]
    responses_retention_days: Option<f64>,
    #[serde(deserialize_with = "num")]
    responses_max_mb: Option<f64>,
    #[serde(deserialize_with = "num")]
    keepalive_s: Option<f64>,
    #[serde(deserialize_with = "num")]
    read_timeout_s: Option<f64>,
    #[serde(deserialize_with = "num")]
    retry_backoff_s: Option<f64>,
    #[serde(deserialize_with = "num")]
    responses_memory_mb: Option<f64>,
    #[serde(deserialize_with = "num")]
    shutdown_grace_s: Option<f64>,
    api_key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TelemetryFile {
    otlp_endpoint: Option<String>,
    service_name: Option<String>,
    #[serde(deserialize_with = "flag")]
    prometheus: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LimitsFile {
    #[serde(deserialize_with = "num")]
    max_concurrent: Option<i64>,
    #[serde(deserialize_with = "num")]
    requests_per_minute: Option<i64>,
    #[serde(deserialize_with = "num")]
    queue_timeout_s: Option<f64>,
    #[serde(deserialize_with = "num")]
    cooldown_on_429_s: Option<f64>,
    #[serde(deserialize_with = "num")]
    max_waiting: Option<i64>,
}

/// `type`, `limits`, and the backend's own options (whatever other keys the table has).
#[derive(Default)]
struct BackendFile {
    type_: Option<String>,
    limits: LimitsFile,
    options: Map<String, Value>,
}

impl<'de> Deserialize<'de> for BackendFile {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BackendFile;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a backend table")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<BackendFile, A::Error> {
                let mut b = BackendFile::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => b.type_ = map.next_value()?,
                        "limits" => b.limits = map.next_value()?,
                        _ => {
                            let v: Value = map.next_value()?;
                            b.options.insert(key, v);
                        }
                    }
                }
                Ok(b)
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ModelFile {
    name: String,
    backend: Option<String>,
    target: String,
    description: String,
    aliases: Vec<String>,
    #[serde(rename = "match")]
    match_: Option<String>,
    #[serde(deserialize_with = "num")]
    max_prompt_chars: Option<i64>,
    #[serde(deserialize_with = "flag")]
    tail_reminder: Option<bool>,
    #[serde(deserialize_with = "num")]
    tool_desc_max: Option<i64>,
    #[serde(deserialize_with = "word")]
    tool_schema: Option<ToolSchema>,
    #[serde(deserialize_with = "flag")]
    followups: Option<bool>,
}

// ---------------------------------------------------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------------------------------------------------

impl Config {
    pub fn load(env: IndexMap<String, String>, root: PathBuf) -> R<Config> {
        let config_file = match env.get("MIDIR_CONFIG").filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => root.join("config").join("midir.toml"),
        };
        Self::load_file(env, config_file, root)
    }

    pub fn load_file(env: IndexMap<String, String>, config_file: PathBuf, root: PathBuf) -> R<Config> {
        let file_name = config_file.display().to_string();
        let is_file = config_file.is_file();
        let raw = if is_file {
            let text = std::fs::read_to_string(&config_file).map_err(|e| ConfigError(format!("{file_name}: cannot read: {e}")))?;
            let table: toml::Table =
                text.parse().map_err(|e: toml::de::Error| ConfigError(format!("{file_name}: cannot read: {}", e.message())))?;
            let value = serde_json::to_value(table).map_err(|e| ConfigError(format!("{file_name}: cannot read: {e}")))?;
            expand(&upgrade_legacy(&config_file, value), &env)
        } else {
            json!({})
        };
        let mut unknown: Vec<String> = vec![];
        let file: FileConfig = serde_path_to_error::deserialize(serde_ignored::Deserializer::new(raw, &mut |path: serde_ignored::Path| {
            unknown.push(path.to_string())
        }))
        .map_err(|e| {
            let path = e.path().to_string();
            ConfigError(format!("{file_name}: {}{}", if path == "." { String::new() } else { format!("{path}: ") }, e.inner()))
        })?;
        for key in unknown {
            tracing::warn!("{file_name}: unknown setting {key:?} ignored (a typo? see config/midir.example.toml)");
        }
        let lookup = EnvLookup(&env);
        let server = server_settings(&file.server, &lookup, &root)?;
        let telemetry = TelemetrySettings {
            otlp_endpoint: lookup
                .str("OTEL_EXPORTER_OTLP_ENDPOINT")
                .or(file.telemetry.otlp_endpoint)
                .unwrap_or_default()
                .trim()
                .to_string(),
            service_name: lookup
                .str("OTEL_SERVICE_NAME")
                .or(file.telemetry.service_name)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "midir".into()),
            prometheus: lookup.flag("MIDIR_PROMETHEUS")?.or(file.telemetry.prometheus).unwrap_or(false),
        };
        let backends = backend_settings(file.backends, &lookup)?;
        let mut cfg = Config {
            env: env.clone(),
            config_file,
            source: if is_file { file_name } else { "env".into() },
            server,
            telemetry,
            backends,
            models: vec![],
            default: Arc::new(ModelSpec::simple("", "", "", "")),
        };
        if file.models.is_empty() {
            cfg.models_from_env()?;
        } else {
            cfg.models_from_file(file.models, file.default_model)?;
        }
        Ok(cfg)
    }

    fn models_from_file(&mut self, models: Vec<ModelFile>, default_model: Option<String>) -> R<()> {
        let file = self.config_file.display().to_string();
        for (i, m) in models.into_iter().enumerate() {
            let name = m.name.trim().to_lowercase();
            let at = format!("{file}: models[{i}] ({})", if name.is_empty() { "?" } else { &name });
            if name.is_empty() || self.models.iter().any(|x| x.name == name) {
                return Err(ConfigError(format!("{at}: needs a unique 'name'")));
            }
            let backend = match m.backend.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                Some(b) => b.to_string(),
                None if self.backends.len() == 1 => self.backends.keys().next().cloned().unwrap_or_default(),
                None => String::new(),
            };
            if !self.backends.contains_key(&backend) {
                let mut names: Vec<&str> = self.backends.keys().map(String::as_str).collect();
                names.sort_unstable();
                return Err(ConfigError(format!("{at}: 'backend' must be one of {} (got {backend:?})", names.join(", "))));
            }
            let target = m.target.trim().to_string();
            if target.is_empty() {
                return Err(ConfigError(format!("{at}: 'target' is empty (unset environment variable?)")));
            }
            let pattern = m.match_.filter(|p| !p.is_empty());
            let match_re = match &pattern {
                Some(p) => Some(Regex::new(&format!("(?i){p}")).map_err(|e| ConfigError(format!("{at}: invalid 'match' regex: {e}")))?),
                None => None,
            };
            self.models.push(Arc::new(ModelSpec {
                name,
                backend,
                target,
                description: m.description,
                aliases: m.aliases.iter().map(|a| a.trim().to_lowercase()).collect(),
                match_re,
                match_pattern: pattern,
                max_prompt_chars: m.max_prompt_chars,
                tail_reminder: m.tail_reminder,
                tool_desc_max: m.tool_desc_max,
                tool_schema: m.tool_schema,
                followups: m.followups,
            }));
        }
        let default_name = default_model.unwrap_or_default().trim().to_lowercase();
        self.default = if default_name.is_empty() {
            self.models[0].clone()
        } else {
            self.models
                .iter()
                .find(|x| x.name == default_name)
                .cloned()
                .ok_or_else(|| ConfigError(format!("{file}: default_model = {default_name:?} is not one of the configured models")))?
        };
        Ok(())
    }

    /// Without [[models]]: STACKSPOT_<MODEL>_AGENT_ID variables define the models and STACKSPOT_DEFAULT_AGENT_ID the
    /// default (unknown model names go there).
    fn models_from_env(&mut self) -> R<()> {
        let backend = if self.backends.contains_key("stackspot") {
            "stackspot".to_string()
        } else {
            self.backends.keys().next().cloned().unwrap_or_default()
        };
        self.models = self
            .env
            .iter()
            .filter_map(|(k, v)| {
                let part = k.strip_prefix("STACKSPOT_")?.strip_suffix("_AGENT_ID")?;
                (!part.is_empty() && part != "DEFAULT" && !v.trim().is_empty())
                    .then(|| Arc::new(ModelSpec::simple(&model_name(part), &backend, v.trim(), "")))
            })
            .collect();
        let default_id = self.env.get("STACKSPOT_DEFAULT_AGENT_ID").map(|s| s.trim()).unwrap_or("");
        self.default = if !default_id.is_empty() {
            Arc::new(ModelSpec::simple(DEFAULT_MODEL_NAME, &backend, default_id, "default agent (STACKSPOT_DEFAULT_AGENT_ID)"))
        } else if let Some(first) = self.models.first() {
            first.clone()
        } else {
            return Err(ConfigError(format!(
                "no models configured: create {} (see config/midir.example.toml) or set STACKSPOT_DEFAULT_AGENT_ID",
                self.config_file.display()
            )));
        };
        Ok(())
    }

    /// The model spec for a requested model name: exact name or alias > `match` regex in file order > exact after
    /// stripping a provider prefix > longest configured name contained in the requested one > default.
    /// The model a requested name routes to (see `find`), or the default model.
    pub fn resolve(&self, model: &str) -> Arc<ModelSpec> {
        self.find(model).unwrap_or_else(|| self.default.clone())
    }

    /// The model a requested name matches: exact name or alias, a `match` regex, exact after a provider prefix, or the
    /// longest configured name it contains; None when it matches none (POST requests then go to the default model).
    pub fn find(&self, model: &str) -> Option<Arc<ModelSpec>> {
        let m = model.trim().to_lowercase();
        let exact = |name: &str| self.models.iter().find(|s| s.name == name || s.aliases.iter().any(|a| a == name)).cloned();
        if let Some(s) = exact(&m) {
            return Some(s);
        }
        if let Some(s) = self.models.iter().find(|s| s.match_re.as_ref().is_some_and(|re| re.is_match(&m))) {
            return Some(s.clone());
        }
        let prefixes: Vec<String> = self.backends.keys().map(|b| format!("{b}-")).chain(["midir-".to_string()]).collect();
        let without_provider = m.split_once('/').filter(|(p, _)| !p.is_empty()).map_or(m.as_str(), |(_, rest)| rest);
        let bare = prefixes.iter().find_map(|p| without_provider.strip_prefix(p.as_str())).unwrap_or(without_provider);
        if let Some(s) = exact(bare) {
            return Some(s);
        }
        let contained = self
            .models
            .iter()
            .filter(|s| m.contains(s.name.as_str()))
            .rev() // among equally long names, the first in the file wins
            .max_by_key(|s| char_len(&s.name))
            .cloned();
        // without [[models]], the default is the one model and answers to its own name
        contained.or_else(|| (self.models.is_empty() && m == self.default.name).then(|| self.default.clone()))
    }

    pub fn exposed_models(&self) -> Vec<Arc<ModelSpec>> {
        if self.models.is_empty() { vec![self.default.clone()] } else { self.models.clone() }
    }

    /// A model's knobs: its own values, else `[server]`'s.
    pub fn knobs(&self, spec: Option<&ModelSpec>) -> Knobs {
        let s = &self.server;
        let server = Knobs {
            max_prompt_chars: s.max_prompt_chars,
            tail_reminder: s.tail_reminder,
            tool_desc_max: s.tool_desc_max,
            tool_schema: s.tool_schema,
            followups: s.followups,
        };
        let Some(spec) = spec else { return server };
        Knobs {
            max_prompt_chars: spec.max_prompt_chars.filter(|n| *n > 0).unwrap_or(s.max_prompt_chars),
            tail_reminder: spec.tail_reminder.unwrap_or(s.tail_reminder),
            tool_desc_max: spec.tool_desc_max.unwrap_or(s.tool_desc_max),
            tool_schema: spec.tool_schema.unwrap_or(s.tool_schema),
            followups: spec.followups.unwrap_or(s.followups),
        }
    }
}

/// Environment overrides: a set, non-empty variable wins over the file.
struct EnvLookup<'a>(&'a IndexMap<String, String>);

impl EnvLookup<'_> {
    fn str(&self, name: &str) -> Option<String> {
        self.0.get(name).filter(|v| !v.is_empty()).cloned()
    }

    fn parse<T: FromStr>(&self, name: &str) -> R<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        match self.str(name) {
            None => Ok(None),
            Some(v) => v.trim().parse().map(Some).map_err(|e| ConfigError(format!("{name}={v:?}: not a valid value ({e})"))),
        }
    }

    fn flag(&self, name: &str) -> R<Option<bool>> {
        match self.str(name) {
            None => Ok(None),
            Some(v) => parse_flag(&v).map(Some).ok_or_else(|| ConfigError(format!("{name}={v:?}: not a boolean (true/false)"))),
        }
    }
}

fn server_settings(f: &ServerFile, env: &EnvLookup, root: &Path) -> R<ServerSettings> {
    let rdir = env.str("MIDIR_RESPONSES_DIR").or_else(|| f.responses_dir.clone()).unwrap_or_else(|| "data/gateway/responses".into());
    let rdir = rdir.trim();
    let responses_dir = match rdir {
        "" => None,
        d if Path::new(d).is_absolute() => Some(PathBuf::from(d)),
        d => Some(root.join(d)),
    };
    let settings = ServerSettings {
        port: env.parse("MIDIR_PORT")?.or(f.port).unwrap_or(18880),
        max_prompt_chars: env.parse("MIDIR_MAX_PROMPT_CHARS")?.or(f.max_prompt_chars).unwrap_or(1_000_000),
        tail_reminder: env.flag("MIDIR_TAIL_REMINDER")?.or(f.tail_reminder).unwrap_or(true),
        tool_desc_max: env.parse("MIDIR_TOOL_DESC_MAX")?.or(f.tool_desc_max).unwrap_or(0),
        tool_schema: env.parse("MIDIR_TOOL_SCHEMA")?.or(f.tool_schema).unwrap_or_default(),
        followups: env.flag("MIDIR_FOLLOWUPS")?.or(f.followups).unwrap_or(true),
        responses_dir,
        responses_retention_days: env.parse("MIDIR_RESPONSES_RETENTION_DAYS")?.or(f.responses_retention_days).unwrap_or(30.0),
        responses_max_mb: env.parse("MIDIR_RESPONSES_MAX_MB")?.or(f.responses_max_mb).unwrap_or(500.0),
        keepalive_s: env.parse("MIDIR_KEEPALIVE_S")?.or(f.keepalive_s).unwrap_or(15.0),
        read_timeout_s: env.parse("MIDIR_READ_TIMEOUT_S")?.or(f.read_timeout_s).unwrap_or(30.0),
        retry_backoff_s: env.parse("MIDIR_RETRY_BACKOFF_S")?.or(f.retry_backoff_s).unwrap_or(1.0),
        responses_memory_mb: env.parse("MIDIR_RESPONSES_MEMORY_MB")?.or(f.responses_memory_mb).unwrap_or(64.0),
        shutdown_grace_s: env.parse("MIDIR_SHUTDOWN_GRACE_S")?.or(f.shutdown_grace_s).unwrap_or(25.0),
        api_key: env.str("MIDIR_API_KEY").or_else(|| f.api_key.clone()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty()),
    };
    let s = &settings;
    in_range("[server] port", f64::from(s.port), 1.0, "at least 1")?;
    in_range("[server] max_prompt_chars", s.max_prompt_chars as f64, 1.0, "at least 1")?;
    in_range("[server] responses_retention_days", s.responses_retention_days, f64::MIN_POSITIVE, "above 0")?;
    for (name, v) in [
        ("[server] responses_max_mb", s.responses_max_mb),
        ("[server] responses_memory_mb", s.responses_memory_mb),
        ("[server] keepalive_s", s.keepalive_s),
        ("[server] retry_backoff_s", s.retry_backoff_s),
        ("[server] shutdown_grace_s", s.shutdown_grace_s),
        ("[server] tool_desc_max", s.tool_desc_max as f64),
    ] {
        in_range(name, v, 0.0, "0 or more")?;
    }
    in_range("[server] read_timeout_s", s.read_timeout_s, f64::MIN_POSITIVE, "above 0")?;
    Ok(settings)
}

/// A setting outside its range is a startup error, not a surprise later (a negative retention deleted every stored
/// response; a zero prompt cap dropped every history turn).
fn in_range(name: &str, value: f64, min: f64, rule: &str) -> R<()> {
    if value.is_nan() || value < min {
        return Err(ConfigError(format!("{name} must be {rule} (got {value})")));
    }
    Ok(())
}

/// The backends. CAVEAT: the MIDIR_MAX_CONCURRENT / MIDIR_REQUESTS_PER_MINUTE / MIDIR_QUEUE_TIMEOUT /
/// MIDIR_COOLDOWN_ON_429 overrides apply to every backend.
fn backend_settings(raw: IndexMap<String, BackendFile>, env: &EnvLookup) -> R<IndexMap<String, BackendSettings>> {
    let raw = if raw.is_empty() { IndexMap::from([("stackspot".to_string(), BackendFile::default())]) } else { raw };
    let mut out = IndexMap::new();
    for (name, b) in raw {
        let limits = LimitSettings {
            max_concurrent: env.parse("MIDIR_MAX_CONCURRENT")?.or(b.limits.max_concurrent).unwrap_or(8),
            requests_per_minute: env.parse("MIDIR_REQUESTS_PER_MINUTE")?.or(b.limits.requests_per_minute).unwrap_or(90),
            queue_timeout_s: env.parse("MIDIR_QUEUE_TIMEOUT")?.or(b.limits.queue_timeout_s).unwrap_or(600.0),
            cooldown_on_429_s: env.parse("MIDIR_COOLDOWN_ON_429")?.or(b.limits.cooldown_on_429_s).unwrap_or(15.0),
            max_waiting: env.parse("MIDIR_MAX_WAITING")?.or(b.limits.max_waiting).unwrap_or(64),
        };
        let type_ = b.type_.filter(|t| !t.is_empty()).unwrap_or_else(|| name.clone());
        let section = format!("[backends.{name}.limits]");
        in_range(&format!("{section} max_concurrent"), limits.max_concurrent as f64, 1.0, "at least 1")?;
        in_range(&format!("{section} requests_per_minute"), limits.requests_per_minute as f64, 0.0, "0 (no limit) or more")?;
        in_range(&format!("{section} queue_timeout_s"), limits.queue_timeout_s, 0.0, "0 or more")?;
        in_range(&format!("{section} cooldown_on_429_s"), limits.cooldown_on_429_s, 0.0, "0 or more")?;
        in_range(&format!("{section} max_waiting"), limits.max_waiting as f64, 0.0, "0 or more")?;
        out.insert(name.clone(), BackendSettings { name, type_, options: b.options, limits });
    }
    Ok(out)
}

/// Every string with `${NAME}` (NAME = [A-Z0-9_]+) replaced by the environment (empty when unset).
fn expand(v: &Value, env: &IndexMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(expand_str(s, env)),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), expand(x, env))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|x| expand(x, env)).collect()),
        other => other.clone(),
    }
}

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

/// GPT_5_1 -> gpt-5.1, GPT_4_1_MINI -> gpt-4.1-mini, O3_MINI -> o3-mini (an underscore between digits becomes a dot).
pub fn model_name(env_part: &str) -> String {
    let low: Vec<char> = env_part.to_lowercase().chars().collect();
    low.iter()
        .enumerate()
        .map(|(i, &c)| match c {
            '_' if i > 0 && low[i - 1].is_ascii_digit() && low.get(i + 1).is_some_and(char::is_ascii_digit) => '.',
            '_' => '-',
            c => c,
        })
        .collect()
}

/// The pre-0.0.1 layout, translated: `default` -> default_model, [stackspot] + [limits] -> [backends.stackspot],
/// [[agents]] (agent_id) -> [[models]] (target).
fn upgrade_legacy(file: &Path, data: Value) -> Value {
    let Value::Object(data) = data else { return data };
    let present = |k: &str| data.get(k).is_some_and(|v| !v.is_null() && v != &json!({}) && v != &json!([]));
    if !(present("agents") || present("stackspot") || data.contains_key("default")) {
        return Value::Object(data);
    }
    tracing::warn!(
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
    let mut backend = Map::from_iter([("type".to_string(), json!("stackspot"))]);
    if let Some(Value::Object(s)) = data.get("stackspot") {
        backend.extend(s.clone());
    }
    if let Some(l @ Value::Object(_)) = data.get("limits") {
        backend.insert("limits".into(), l.clone());
    }
    if let Value::Object(b) = out.entry("backends").or_insert_with(|| json!({})) {
        b.entry("stackspot").or_insert(Value::Object(backend));
    }
    if let Some(Value::Array(agents)) = data.get("agents") {
        let models = agents
            .iter()
            .filter_map(Value::as_object)
            .map(|a| {
                let mut m: Map<String, Value> =
                    a.iter().filter(|(k, _)| k.as_str() != "agent_id").map(|(k, v)| (k.clone(), v.clone())).collect();
                m.insert("backend".into(), json!("stackspot"));
                m.insert("target".into(), a.get("agent_id").cloned().unwrap_or(json!("")));
                Value::Object(m)
            })
            .collect();
        out.insert("models".into(), Value::Array(models));
    }
    Value::Object(out)
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

    #[test]
    fn numbers_flags_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("midir.toml");
        let toml = "[server]\nkeepalive_s = 0.05\nresponses_max_mb = \"12\"\ntail_reminder = \"off\"\nrespones_dir = \"x\"\n\n[backends.stackspot]\nrealm = \"r\"\n[backends.stackspot.limits]\nrequests_per_minut = 5\nmax_concurrent = 3\n\n[[models]]\nname = \"m\"\ntarget = \"T\"\nmax_prompt_chars = 1000.0\n";
        std::fs::write(&file, toml).unwrap();
        let err = Config::load_file(IndexMap::new(), file.clone(), dir.path().into()).err().unwrap();
        assert!(err.0.contains("models[0].max_prompt_chars") || err.0.contains("max_prompt_chars"), "{}", err.0);
        std::fs::write(&file, toml.replace("1000.0", "1000")).unwrap();
        let cfg = Config::load_file(IndexMap::new(), file, dir.path().into()).unwrap();
        assert_eq!((cfg.server.keepalive_s, cfg.server.responses_max_mb, cfg.server.tail_reminder), (0.05, 12.0, false));
        assert_eq!(cfg.backends["stackspot"].limits.max_concurrent, 3);
        assert_eq!(cfg.backends["stackspot"].options["realm"], "r");
        assert_eq!(cfg.models[0].max_prompt_chars, Some(1000));
    }

    #[test]
    fn per_model_knobs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("midir.toml");
        let toml = "[server]\nmax_prompt_chars = 5000\ntool_desc_max = 80\n[backends.stackspot]\n[[models]]\nname = \"a\"\ntarget = \"A\"\nmax_prompt_chars = 100\ntail_reminder = false\nfollowups = false\n[[models]]\nname = \"b\"\ntarget = \"B\"\nmax_prompt_chars = 0\n";
        std::fs::write(&file, toml).unwrap();
        let knobs = |max_prompt_chars, tail_reminder, followups| Knobs {
            max_prompt_chars,
            tail_reminder,
            tool_desc_max: 80,
            tool_schema: ToolSchema::Json,
            followups,
        };
        let cfg = Config::load_file(IndexMap::new(), file.clone(), dir.path().into()).unwrap();
        assert_eq!(cfg.knobs(Some(&cfg.resolve("a"))), knobs(100, false, false));
        assert_eq!(cfg.knobs(Some(&cfg.resolve("b"))), knobs(5000, true, true)); // 0 means the server's cap
        assert_eq!(cfg.knobs(None), knobs(5000, true, true));
        // MIDIR_FOLLOWUPS turns them off for the models that do not set their own
        let env = IndexMap::from([("MIDIR_FOLLOWUPS".to_string(), "0".to_string())]);
        let cfg = Config::load_file(env, file, dir.path().into()).unwrap();
        assert!(!cfg.knobs(Some(&cfg.resolve("b"))).followups);
    }

    #[test]
    fn expansion() {
        let env = IndexMap::from([("A".to_string(), "x".to_string())]);
        assert_eq!(expand_str("${A}-${B}-${lower}-$A", &env), "x--${lower}-$A");
    }
}
