//! Logging: `tracing` events printed to stderr as `HH:MM:SS LEVEL target: message` in local time. `--debug` adds the
//! DEBUG level (which includes the full rendered prompts).

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
    let level = if debug { tracing::Level::DEBUG } else { tracing::Level::INFO };
    tracing_subscriber::fmt().with_max_level(level).with_timer(LocalClock).with_ansi(false).with_writer(std::io::stderr).init();
}
