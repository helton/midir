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
mod log;
mod otlp;
mod protocols;
mod store;
mod telemetry;
mod text;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser;
use tower::Layer;
use tower_http::normalize_path::NormalizePathLayer;

const ABOUT: &str = "Midir: OpenAI- and Anthropic-compatible gateway for LLM backends.";

const DETAILS: &str = "Endpoints (http://<host>:<port>)
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
MIDIR_RESPONSES_DIR, MIDIR_RESPONSES_RETENTION_DAYS, MIDIR_RESPONSES_MAX_MB, MIDIR_KEEPALIVE_S, MIDIR_RETRY_BACKOFF_S,
MIDIR_MAX_CONCURRENT, MIDIR_REQUESTS_PER_MINUTE, MIDIR_QUEUE_TIMEOUT, MIDIR_COOLDOWN_ON_429,
OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME, and per backend (StackSpot): STACKSPOT_REALM, STACKSPOT_CLIENT_ID,
STACKSPOT_CLIENT_SECRET, STACKSPOT_CA_BUNDLE. Without [[models]], STACKSPOT_DEFAULT_AGENT_ID and
STACKSPOT_<MODEL>_AGENT_ID define the models (STACKSPOT_GPT_5_1_AGENT_ID -> model \"gpt-5.1\").
MIDIR_NO_BANNER=1 skips the startup banner.

No authentication: local use only. Do not expose it on a network without something in front of it.";

/// `--version` shows the full build identity (release, snapshot, local image or source build).
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(buildinfo::full_version)
}

#[derive(Parser)]
#[command(name = "midir", about = ABOUT, after_help = DETAILS, version = version())]
struct Args {
    /// Port to listen on [default: MIDIR_PORT, [server] port or 18880]
    #[arg(long)]
    port: Option<u16>,
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Log the full rendered prompts to stderr (they contain everything clients send)
    #[arg(long)]
    debug: bool,
    /// GET http://127.0.0.1:<port>/health and exit 0 (healthy) or 1, for images without a shell
    #[arg(long)]
    healthcheck: bool,
}

/// The startup banner: the name, a vertical rule, and the version (with the kind of build when it is not a release).
fn banner() -> String {
    let b = buildinfo::build();
    let name = "M  I  D  I  R";
    let blank = " ".repeat(name.chars().count());
    let label = b.label();
    let version = format!("v{}{}", b.full_version(), if label.is_empty() { String::new() } else { format!(" · {label}") });
    let lines = [
        (blank.as_str(), version.as_str()),
        (name, "an LLM gateway for agent platforms"),
        (blank.as_str(), "OpenAI · Anthropic ⇄ text-only agents"),
    ];
    let body: Vec<String> = lines.iter().map(|(n, t)| format!("  {n}  │  {t}")).collect();
    format!("\n{}\n\n", body.join("\n"))
}

fn print_banner() {
    let off =
        std::env::var("MIDIR_NO_BANNER").map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(false);
    if !off {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(banner().as_bytes());
        let _ = err.flush();
    }
}

/// Variables from `.env` in the working directory; the environment wins over the file.
fn load_dotenv(dir: &Path) {
    let path = dir.join(".env");
    if path.is_file() {
        if let Err(e) = dotenvy::from_path(&path) {
            eprintln!("warning: {}: {e}", path.display());
        }
    }
}

/// `--healthcheck`: GET /health on the configured port (for images without a shell or curl).
fn healthcheck(args: &Args, cwd: &Path) -> ExitCode {
    load_dotenv(cwd);
    let port = args.port.or_else(|| std::env::var("MIDIR_PORT").ok().and_then(|p| p.trim().parse().ok())).or_else(|| {
        let file = std::env::var("MIDIR_CONFIG")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join("config").join("midir.toml"));
        let t: toml::Table = std::fs::read_to_string(file).ok()?.parse().ok()?;
        u16::try_from(t.get("server")?.get("port")?.as_integer()?).ok()
    });
    let port = port.unwrap_or(18880);
    let probe = || -> std::io::Result<bool> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200"))
    };
    if probe().unwrap_or(false) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

fn main() -> ExitCode {
    let _ = buildinfo::build(); // detected before .env, so .env cannot change it
    let args = Args::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if args.healthcheck {
        return healthcheck(&args, &cwd);
    }
    print_banner();
    log::init(args.debug);
    tracing::info!("midir v{} starting", buildinfo::full_version());
    load_dotenv(&cwd);
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
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let tel = Arc::new(telemetry::Telemetry::new(&cfg.telemetry.otlp_endpoint, &cfg.telemetry.service_name));
    let gw = match gateway::Gateway::new(cfg.clone(), tel.clone()).and_then(|g| g.validate().map(|_| g)) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            eprintln!("{e}");
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
    tracing::info!(
        "midir {} at http://{}:{port}/v1 (chat/completions, responses, messages); config {}; backends {}; default {}; models: {} (max prompt {} chars)",
        buildinfo::full_version(),
        args.host,
        cfg.source,
        backends.join(", "),
        cfg.default.name,
        models.join(", "),
        cfg.server.max_prompt_chars
    );
    let listener = match tokio::net::TcpListener::bind((args.host.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot listen on {}:{port}: {e}", args.host);
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
    // "/v1/chat/completions/" is "/v1/chat/completions"
    let service = NormalizePathLayer::trim_trailing_slash().layer(app::router(Arc::new(app::App { gateway: gw.clone() })));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let make = axum::ServiceExt::<axum::extract::Request>::into_make_service(service);
        let _ = axum::serve(listener, make)
            .tcp_nodelay(true)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await;
    });
    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    tel.shutdown().await; // flush pending spans and metrics on a graceful stop (SIGTERM)
    ExitCode::SUCCESS
}
