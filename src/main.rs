//! midir: OpenAI- and Anthropic-compatible gateway for LLM backends.
//! Command line: arguments, the startup banner, .env, configuration, and the server with a graceful stop that flushes
//! telemetry (SIGTERM).

mod app;
mod backends;
mod buildinfo;
mod canonical;
mod config;
mod emulation;
mod errors;
mod gateway;
mod limiter;
#[macro_use]
mod log;
mod otlp;
mod protocols;
mod py;
mod store;
mod telemetry;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;

const LOG: &str = "midir";

const DOC: &str = "midir: OpenAI- and Anthropic-compatible gateway for LLM backends.

Endpoints (http://127.0.0.1:{port})
  POST /v1/chat/completions       OpenAI Chat Completions (streaming or not, tools, response_format)
  POST /v1/responses              OpenAI Responses (streaming or not, function/custom/namespace tools, previous_response_id)
  GET  /v1/responses/{id}         stored response
  POST /v1/messages               Anthropic Messages (streaming or not, tools)
  POST /v1/messages/count_tokens  Anthropic count_tokens (estimate)
  GET  /v1/models                 configured models (unknown names go to the default model)
  GET  /health, GET /ready        liveness (mapping, queues) and readiness (backend credentials and network)

Configuration: config/midir.toml under the working directory (or MIDIR_CONFIG=<path>); format in
config/midir.example.toml. Secrets live in .env (working directory) and are referenced as ${NAME}. Environment
variables win over the file: MIDIR_PORT, MIDIR_MAX_PROMPT_CHARS, MIDIR_TAIL_REMINDER, MIDIR_TOOL_DESC_MAX,
MIDIR_RESPONSES_DIR, MIDIR_RESPONSES_RETENTION_DAYS, MIDIR_RESPONSES_MAX_MB, MIDIR_KEEPALIVE_S, MIDIR_RETRY_BACKOFF_S, MIDIR_MAX_CONCURRENT,
MIDIR_REQUESTS_PER_MINUTE, MIDIR_QUEUE_TIMEOUT, MIDIR_COOLDOWN_ON_429, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME,
and per backend (StackSpot): STACKSPOT_REALM, STACKSPOT_CLIENT_ID, STACKSPOT_CLIENT_SECRET, STACKSPOT_CA_BUNDLE.
Without [[models]], STACKSPOT_DEFAULT_AGENT_ID and STACKSPOT_<MODEL>_AGENT_ID define the models
(STACKSPOT_GPT_5_1_AGENT_ID -> model \"gpt-5.1\"). MIDIR_NO_BANNER=1 skips the startup banner.

No authentication: local use only. Do not expose on a network without something in front of it.
";

const USAGE: &str = "usage: midir [-h] [--port PORT] [--host HOST] [--debug] [--version] [--healthcheck]";

struct Args {
    port: Option<i64>,
    host: String,
    debug: bool,
    healthcheck: bool,
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("{USAGE}\nmidir: error: {msg}");
    ExitCode::from(2)
}

fn parse_args() -> Result<Args, ExitCode> {
    let mut args = Args { port: None, host: "127.0.0.1".into(), debug: false, healthcheck: false };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let options = ["--port", "--host", "--debug", "--version", "--help", "--healthcheck"];
    while i < argv.len() {
        let a = &argv[i];
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        // argparse accepts unambiguous prefixes (--deb for --debug)
        let name = if name.starts_with("--") && !options.contains(&name.as_str()) {
            let hits: Vec<&&str> = options.iter().filter(|o| o.starts_with(name.as_str())).collect();
            if hits.len() == 1 {
                hits[0].to_string()
            } else {
                name
            }
        } else {
            name
        };
        let mut value = |what: &str| -> Result<String, ExitCode> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            i += 1;
            argv.get(i)
                .filter(|v| !v.starts_with('-') || v.parse::<i64>().is_ok())
                .cloned()
                .ok_or_else(|| usage_error(&format!("argument {what}: expected one argument")))
        };
        match name.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}\n\n{DOC}\noptions:\n  -h, --help     show this help message and exit\n  --port PORT    default: MIDIR_PORT, [server] port or 18880\n  --host HOST\n  --debug        log the full rendered prompt to stderr (contains everything clients send)\n  --version      show program's version number and exit\n  --healthcheck  GET http://127.0.0.1:<port>/health and exit 0 (healthy) or 1 (for images without a shell)");
                return Err(ExitCode::SUCCESS);
            }
            "--version" => {
                println!("midir {}", buildinfo::full_version());
                return Err(ExitCode::SUCCESS);
            }
            "--debug" => args.debug = true,
            "--healthcheck" => args.healthcheck = true,
            "--port" => {
                let v = value("--port")?;
                match py::text::parse_int(&v) {
                    Some(p) => args.port = Some(p),
                    None => return Err(usage_error(&format!("argument --port: invalid int value: {}", py::text::repr_str(&v)))),
                }
            }
            "--host" => args.host = value("--host")?,
            _ => return Err(usage_error(&format!("unrecognized arguments: {}", argv[i..].join(" ")))),
        }
        i += 1;
    }
    Ok(args)
}

/// The startup banner: the name, a vertical rule, and the version (with the kind of build when it is not a release).
fn banner() -> String {
    let b = buildinfo::build();
    let name = "M  I  D  I  R";
    let blank = " ".repeat(name.chars().count());
    let left = [blank.as_str(), name, blank.as_str()];
    let label = b.label();
    let version = format!("v{}{}", b.full_version(), if label.is_empty() { String::new() } else { format!(" · {label}") });
    let side = [version.as_str(), "an LLM gateway for agent platforms", "OpenAI · Anthropic ⇄ text-only agents"];
    let lines: Vec<String> = left.iter().zip(side.iter()).map(|(n, t)| format!("  {n}  │  {t}")).collect();
    format!("\n{}\n\n", lines.join("\n"))
}

fn print_banner() {
    let v = std::env::var("MIDIR_NO_BANNER").unwrap_or_default().to_lowercase();
    if ["1", "true", "yes", "on"].contains(&v.as_str()) {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(banner().as_bytes());
    let _ = err.flush();
}

/// python-dotenv's load_dotenv(override=False): KEY=value lines, `export`, quotes, comments, ${VAR} expansion.
fn load_dotenv(path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let mut values: IndexMap<String, String> = IndexMap::new();
    for line in raw.lines() {
        let mut l = line.trim_start();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(rest) = l.strip_prefix("export ") {
            l = rest.trim_start();
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let key = k.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if key.is_empty() {
            continue;
        }
        let v = v.trim_start();
        let (value, expand) = if let Some(rest) = v.strip_prefix('\'') {
            (rest.split('\'').next().unwrap_or("").replace("\\'", "'"), false)
        } else if let Some(rest) = v.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = rest.chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some(o) => out.push(o),
                        None => {}
                    },
                    o => out.push(o),
                }
            }
            (out, true)
        } else {
            let cut = v.find(" #").map_or(v, |i| &v[..i]);
            (cut.trim_end().to_string(), true)
        };
        let value = if expand { expand_vars(&value, &values) } else { value };
        values.insert(key, value);
    }
    for (k, v) in values {
        if std::env::var_os(&k).is_none() {
            std::env::set_var(&k, v);
        }
    }
}

fn expand_vars(s: &str, values: &IndexMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n, d),
            None => (inner, ""),
        };
        let v = std::env::var(name).ok().or_else(|| values.get(name).cloned()).unwrap_or_else(|| default.to_string());
        out.push_str(&v);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `--healthcheck`: GET /health on the configured port (for images without a shell or curl).
fn healthcheck(args: &Args) -> ExitCode {
    load_dotenv(&std::env::current_dir().unwrap_or_default().join(".env"));
    let port =
        args.port.filter(|p| *p != 0).or_else(|| std::env::var("MIDIR_PORT").ok().and_then(|p| py::text::parse_int(&p))).or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            let file = std::env::var("MIDIR_CONFIG")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| cwd.join("config").join("midir.toml"));
            let t: toml::Table = std::fs::read_to_string(file).ok()?.parse().ok()?;
            t.get("server")?.get("port")?.as_integer()
        });
    let port = port.unwrap_or(18880);
    let ok = (|| -> std::io::Result<bool> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port as u16));
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200"))
    })()
    .unwrap_or(false);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async { if let Some(t) = term.as_mut() { t.recv().await; } else { std::future::pending::<()>().await } } => {}
        _ = async { if let Some(t) = int.as_mut() { t.recv().await; } else { std::future::pending::<()>().await } } => {}
    }
}

fn main() -> ExitCode {
    let _ = buildinfo::build(); // detected before .env, so .env cannot change it
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return code,
    };
    if args.healthcheck {
        return healthcheck(&args);
    }
    print_banner();
    log::set_debug(args.debug);
    info!(LOG, "midir v{} starting (Rust)", buildinfo::full_version());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_dotenv(&cwd.join(".env"));
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(args, cwd))
}

async fn run(args: Args, cwd: PathBuf) -> ExitCode {
    let cfg = match config::Config::load(config::environment(), cwd) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("{}", e.0);
            return ExitCode::FAILURE;
        }
    };
    let tel = Arc::new(telemetry::Telemetry::new(&cfg.telemetry.otlp_endpoint, &cfg.telemetry.service_name));
    let gw = match gateway::Gateway::new(cfg.clone(), tel.clone()).and_then(|g| g.validate().map(|_| g)) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            eprintln!("{}", e.0);
            return ExitCode::FAILURE;
        }
    };
    let port = args.port.filter(|p| *p != 0).unwrap_or(cfg.server.port);
    let models: Vec<String> = cfg
        .exposed_models()
        .iter()
        .map(|m| {
            format!("{}->{}:{}", m.name, m.backend, gw.backends.get(&m.backend).map_or(m.target.clone(), |b| b.describe_target(&m.target)))
        })
        .collect();
    let backends: Vec<String> = gw.backends.iter().map(|(n, b)| format!("{n} ({})", b.type_)).collect();
    info!(
        LOG,
        "midir {} at http://{}:{port}/v1 (chat/completions, responses, messages); config {}; backends {}; default {}; models: {} (max prompt {} chars)",
        buildinfo::full_version(),
        args.host,
        cfg.source,
        backends.join(", "),
        cfg.default.name,
        models.join(", "),
        cfg.server.max_prompt_chars
    );
    let Ok(port16) = u16::try_from(port) else {
        error!(LOG, "invalid port {port}");
        return ExitCode::FAILURE;
    };
    let listener = match tokio::net::TcpListener::bind((args.host.as_str(), port16)).await {
        Ok(l) => l,
        Err(e) => {
            error!(
                LOG,
                "[Errno {}] error while attempting to bind on address ({}, {port}): {e}",
                e.raw_os_error().unwrap_or(0),
                py::text::repr_str(&args.host)
            );
            return ExitCode::FAILURE;
        }
    };
    tel.spawn_exporter();
    if gw.store.dir.is_some() {
        let store = gw.store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                let s = store.clone();
                let _ = tokio::task::spawn_blocking(move || s.purge()).await;
            }
        });
    }
    let router = app::router(Arc::new(app::App { gateway: gw.clone() }));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .tcp_nodelay(true)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await;
    });
    shutdown_signal().await;
    info!(LOG, "shutting down");
    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    tel.shutdown().await; // flush pending spans and metrics on a graceful stop (SIGTERM)
    ExitCode::SUCCESS
}
