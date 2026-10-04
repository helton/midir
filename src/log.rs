//! Logging: `tracing` events on stderr as `HH:MM:SS LEVEL target: message` in local time, or one JSON object per line
//! with `MIDIR_LOG_FORMAT=json`. The level is INFO; `--debug` adds Midir's DEBUG events (which include the full
//! rendered prompts); `MIDIR_LOG` takes a filter (`info,midir::store=debug`) and wins over both.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Local wall-clock time (containers follow TZ). `localtime_r` is thread-safe, unlike reading the offset once.
struct LocalClock;

impl FormatTime for LocalClock {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        // SAFETY: `time` with a null pointer only returns the time; `localtime_r` writes only the struct we own.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe {
            let now = libc::time(std::ptr::null_mut());
            libc::localtime_r(&now, &mut tm);
        }
        write!(w, "{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

pub fn init(debug: bool) {
    let default = if debug { "info,midir=debug" } else { "info" };
    let filter = std::env::var("MIDIR_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| EnvFilter::try_new(v.trim()).map_err(|e| eprintln!("warning: MIDIR_LOG: {e}; using {default:?}")).ok())
        .unwrap_or_else(|| EnvFilter::new(default));
    let json = std::env::var("MIDIR_LOG_FORMAT").is_ok_and(|v| v.trim().eq_ignore_ascii_case("json"));
    let logs = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::io::stderr);
    if json {
        logs.json().init();
    } else {
        logs.with_timer(LocalClock).init();
    }
}
