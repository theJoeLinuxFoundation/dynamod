//! Messages on standard error, prefixed with the program's name.
//!
//! The level is taken from `$SYSTEMD_LOG_LEVEL` (a name such as `debug`,
//! `info`, `notice`, `warning`, `err`, or a number 0-7), as systemd's tools
//! do; the default is `info`.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

pub const ERR: u8 = 3;
pub const WARNING: u8 = 4;
pub const NOTICE: u8 = 5;
pub const INFO: u8 = 6;
pub const DEBUG: u8 = 7;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);
static PROG: OnceLock<String> = OnceLock::new();

/// Set the program name shown before each message and read the level.
pub fn init(prog: &str) {
    let _ = PROG.set(prog.to_string());
    if let Ok(v) = std::env::var("SYSTEMD_LOG_LEVEL") {
        // "debug,console:info" and the like: the first plain value counts.
        if let Some(l) = v
            .split(',')
            .find(|s| !s.contains(':'))
            .and_then(parse_level)
        {
            set_level(l);
        }
    }
}

pub fn parse_level(s: &str) -> Option<u8> {
    Some(match s.trim() {
        "emerg" => 0,
        "alert" => 1,
        "crit" => 2,
        "err" | "error" => ERR,
        "warning" | "warn" => WARNING,
        "notice" => NOTICE,
        "info" => INFO,
        "debug" => DEBUG,
        n => match n.parse::<u8>() {
            Ok(v) if v <= 7 => v,
            _ => return None,
        },
    })
}

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

pub fn prog() -> &'static str {
    PROG.get().map(|s| s.as_str()).unwrap_or("dynamod")
}

pub fn emit(level: u8, args: std::fmt::Arguments<'_>) {
    if enabled(level) {
        eprintln!("{}: {}", prog(), args);
    }
}

#[macro_export]
macro_rules! log_error {
    ($($t:tt)*) => { $crate::log::emit($crate::log::ERR, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_warning {
    ($($t:tt)*) => { $crate::log::emit($crate::log::WARNING, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_notice {
    ($($t:tt)*) => { $crate::log::emit($crate::log::NOTICE, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_info {
    ($($t:tt)*) => { $crate::log::emit($crate::log::INFO, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($($t:tt)*) => { $crate::log::emit($crate::log::DEBUG, format_args!($($t)*)) };
}
