//! midir: OpenAI- and Anthropic-compatible gateway for LLM backends.
//! Command line: arguments, the startup banner, .env, configuration, and the server with a graceful stop (SIGTERM):
//! no new connections, in-flight requests finish (up to shutdown_grace_s), then telemetry is flushed.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser;
use midir::{app, buildinfo, config, gateway, log, telemetry};
use tower::Layer;
use tower_http::normalize_path::NormalizePathLayer;

/// jemalloc: the request path allocates many short-lived buffers from many threads, where the system allocators
/// (musl's above all, in the static image) contend.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc's settings, read when it starts (`_RJEM_MALLOC_CONF` overrides them): one background thread gives memory
/// freed after a burst back to the system within about a second. Without it the process keeps its peak (300 MB after
/// 32 concurrent large requests) until the next allocation.
#[repr(transparent)]
struct MallocConf(*const std::ffi::c_char);
// SAFETY: a pointer to a static, immutable C string
unsafe impl Sync for MallocConf {}
#[unsafe(export_name = "_rjem_malloc_conf")]
static MALLOC_CONF: MallocConf =
    MallocConf(c"background_thread:true,max_background_threads:1,dirty_decay_ms:1000,muzzy_decay_ms:0".as_ptr());

/// Async worker threads unless TOKIO_WORKER_THREADS says otherwise (tokio's default is one per core). Each thread
/// holds its own allocator arena, so memory under load grows with them; four serve far more than a backend allows
/// (measured: 644 against 693 requests/s with 32 threads, peak 104 against 297 MB).
const WORKER_THREADS: usize = 4;

const ABOUT: &str = "Midir: OpenAI- and Anthropic-compatible gateway for LLM backends.";

const DETAILS: &str = "Endpoints (http://<host>:<port>)
  POST /v1/chat/completions       OpenAI Chat Completions (streaming or not, tools, response_format)
  POST /v1/responses              OpenAI Responses (streaming or not, function/custom/namespace tools, previous_response_id)
  GET  /v1/responses/{id}         stored response
  POST /v1/messages               Anthropic Messages (streaming or not, tools)
  POST /v1/messages/count_tokens  Anthropic count_tokens (estimate)
  GET  /v1/models                 configured models (unknown names go to the default model)
  GET  /health, GET /ready        liveness (mapping, queues) and readiness (backend credentials and network)
  GET  /metrics                   Prometheus metrics (with [telemetry] prometheus = true)

Configuration: config/midir.toml under the working directory (or MIDIR_CONFIG=<path>); format in
config/midir.example.toml. Secrets live in .env (working directory) and are referenced as ${NAME}. Environment
variables win over the file: MIDIR_PORT, MIDIR_API_KEY, MIDIR_MAX_PROMPT_CHARS, MIDIR_TAIL_REMINDER,
MIDIR_TOOL_DESC_MAX, MIDIR_FOLLOWUPS, MIDIR_RESPONSES_DIR, MIDIR_RESPONSES_RETENTION_DAYS, MIDIR_RESPONSES_MAX_MB,
MIDIR_RESPONSES_MEMORY_MB, MIDIR_KEEPALIVE_S, MIDIR_READ_TIMEOUT_S, MIDIR_RETRY_BACKOFF_S, MIDIR_SHUTDOWN_GRACE_S, MIDIR_PROMETHEUS,
OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME; for every backend, MIDIR_MAX_CONCURRENT, MIDIR_REQUESTS_PER_MINUTE,
MIDIR_QUEUE_TIMEOUT and MIDIR_COOLDOWN_ON_429; for every StackSpot backend, STACKSPOT_REALM, STACKSPOT_CLIENT_ID,
STACKSPOT_CLIENT_SECRET, STACKSPOT_CA_BUNDLE, STACKSPOT_IDM_BASE_URL and STACKSPOT_AGENT_BASE_URL (a second StackSpot
account goes in its [backends.<name>] options, with these unset). Without [[models]], STACKSPOT_DEFAULT_AGENT_ID and
STACKSPOT_<MODEL>_AGENT_ID define the models (STACKSPOT_GPT_5_1_AGENT_ID -> model \"gpt-5.1\").
Logs: MIDIR_LOG=<filter> (e.g. info,midir::store=debug), MIDIR_LOG_FORMAT=json. MIDIR_NO_BANNER=1 skips the banner.
TOKIO_WORKER_THREADS=<n> changes the async worker threads (default 4, fewer on smaller machines).

Without MIDIR_API_KEY there is no authentication: keep the port local, or set a key and give it to the clients
(Authorization: Bearer <key> or x-api-key: <key>).";

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
    if path.is_file()
        && let Err(e) = dotenvy::from_path(&path)
    {
        eprintln!("warning: {}: {e}", path.display());
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
        match t.get("server")?.get("port")? {
            toml::Value::Integer(n) => u16::try_from(*n).ok(),
            // a string, as the configuration allows: "18880" or "${PORT}"
            toml::Value::String(s) => config::expand_str(s, &config::environment()).trim().parse().ok(),
            _ => None,
        }
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
    if probe().unwrap_or(false) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Connections a single Midir serves at once; beyond it new ones are closed until one ends.
const MAX_CONNECTIONS: usize = 4096;

/// Accept connections and serve them (HTTP/1 and HTTP/2) until `stop`, then let the open ones finish. Unlike
/// `axum::serve`, the connection builder gets a timer, so a request whose headers do not arrive within `read_timeout`
/// is closed instead of holding the connection forever.
type Service = tower_http::timeout::RequestBodyTimeout<tower_http::normalize_path::NormalizePath<axum::Router>>;

async fn serve(listener: tokio::net::TcpListener, service: Service, read_timeout: Duration, stop: tokio::sync::oneshot::Receiver<()>) {
    use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut stop = stop;
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = &mut stop => break,
        };
        let tcp = match accepted {
            Ok((tcp, _)) => tcp,
            Err(e) => {
                // out of file descriptors and the like: wait a moment instead of spinning
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("{MAX_CONNECTIONS} connections open: closing a new one");
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
        builder.http1().timer(TokioTimer::new()).header_read_timeout(read_timeout);
        builder.http2().timer(TokioTimer::new());
        let conn = builder
            .serve_connection_with_upgrades(TokioIo::new(tcp), hyper_util::service::TowerToHyperService::new(service.clone()))
            .into_owned();
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            let _ = conn.await;
            drop(slot);
        });
    }
    graceful.shutdown().await;
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
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
    load_dotenv(&cwd); // before the banner and the log, which read MIDIR_NO_BANNER, MIDIR_LOG and MIDIR_LOG_FORMAT
    print_banner();
    log::init(args.debug);
    tracing::info!("midir v{} starting", buildinfo::full_version());
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        builder.worker_threads(std::thread::available_parallelism().map_or(WORKER_THREADS, |n| n.get().min(WORKER_THREADS)));
    }
    let runtime = match builder.enable_all().build() {
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
    let tel = Arc::new(telemetry::Telemetry::new(&cfg.telemetry.otlp_endpoint, &cfg.telemetry.service_name, cfg.telemetry.prometheus));
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
    let backends: Vec<String> = gw.backends.iter().map(|(n, b)| format!("{n} ({})", b.kind())).collect();
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
    let read_timeout = Duration::from_secs_f64(cfg.server.read_timeout_s.max(0.1));
    // a body that stalls longer than the read timeout between chunks ends the request
    let service = tower_http::timeout::RequestBodyTimeoutLayer::new(read_timeout).layer(service);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, service, read_timeout, stop_rx));
    if cfg.server.api_key.is_none() && !args.host.starts_with("127.") && args.host != "localhost" && args.host != "::1" {
        // in a container this is the usual case (compose publishes the port on 127.0.0.1 only): information, not alarm
        tracing::info!(
            "no API key (MIDIR_API_KEY): whoever reaches {}:{port} can use the backend account; keep the port private",
            args.host
        );
    }
    shutdown_signal().await;
    let grace = cfg.server.shutdown_grace_s.max(0.0);
    tracing::info!("shutting down: no new connections; in-flight requests get up to {grace}s to finish");
    let _ = stop_tx.send(());
    if tokio::time::timeout(Duration::from_secs_f64(grace), server).await.is_err() {
        tracing::warn!("shutdown grace period ({grace}s) over: cutting the requests still running");
    }
    tel.shutdown().await; // flush pending spans and metrics on a graceful stop (SIGTERM)
    ExitCode::SUCCESS
}
