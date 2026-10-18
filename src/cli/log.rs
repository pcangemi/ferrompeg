//! Leveled logging to standard error.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Quiet = 0,
    Error = 1,
    Warning = 2,
    Info = 3,
    Verbose = 4,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "quiet" | "-8" | "panic" | "fatal" => LogLevel::Quiet,
            "error" | "16" => LogLevel::Error,
            "warning" | "24" => LogLevel::Warning,
            "info" | "32" => LogLevel::Info,
            "verbose" | "debug" | "trace" | "40" | "48" => LogLevel::Verbose,
            _ => return None,
        })
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

pub fn set_level(level: LogLevel) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: LogLevel) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn error(msg: impl AsRef<str>) {
    if enabled(LogLevel::Error) {
        eprintln!("{}", msg.as_ref());
    }
}

pub fn warn(msg: impl AsRef<str>) {
    if enabled(LogLevel::Warning) {
        eprintln!("{}", msg.as_ref());
    }
}

pub fn info(msg: impl AsRef<str>) {
    if enabled(LogLevel::Info) {
        eprintln!("{}", msg.as_ref());
    }
}

pub fn verbose(msg: impl AsRef<str>) {
    if enabled(LogLevel::Verbose) {
        eprintln!("{}", msg.as_ref());
    }
}
