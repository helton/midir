//! Logging format: `HH:MM:SS LEVEL logger.name: message` on stderr (local time).

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Warning = 30,
    Error = 40,
}

static THRESHOLD: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_debug(debug: bool) {
    THRESHOLD.store(if debug { Level::Debug } else { Level::Info } as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 >= THRESHOLD.load(Ordering::Relaxed)
}

fn clock() -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: localtime_r only writes the struct we own.
    unsafe {
        libc::localtime_r(&now, &mut tm);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn write(level: Level, name: &str, msg: &str) {
    if !enabled(level) {
        return;
    }
    let lv = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warning => "WARNING",
        Level::Error => "ERROR",
    };
    let line = format!("{} {lv} {name}: {msg}\n", clock());
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! debug {
    ($name:expr, $($arg:tt)*) => { if $crate::log::enabled($crate::log::Level::Debug) { $crate::log::write($crate::log::Level::Debug, $name, &format!($($arg)*)) } };
}
#[macro_export]
macro_rules! info {
    ($name:expr, $($arg:tt)*) => { if $crate::log::enabled($crate::log::Level::Info) { $crate::log::write($crate::log::Level::Info, $name, &format!($($arg)*)) } };
}
#[macro_export]
macro_rules! warn {
    ($name:expr, $($arg:tt)*) => { $crate::log::write($crate::log::Level::Warning, $name, &format!($($arg)*)) };
}
#[macro_export]
macro_rules! error {
    ($name:expr, $($arg:tt)*) => { $crate::log::write($crate::log::Level::Error, $name, &format!($($arg)*)) };
}
