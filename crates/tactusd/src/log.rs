//! Lightweight stderr/journald logging for the non-RT startup and async control plane.

use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;

static HAS_JOURNAL_STREAM: OnceLock<bool> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) enum LogLevel {
    Info,
    Warn,
    Error,
}

pub(crate) fn write_log(level: LogLevel, args: fmt::Arguments<'_>) {
    let journal = *HAS_JOURNAL_STREAM
        .get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some_and(|v| !v.is_empty()));
    let prefix = match (journal, level) {
        (true, LogLevel::Info) => "<6>",
        (true, LogLevel::Warn) => "<4>",
        (true, LogLevel::Error) => "<3>",
        (false, LogLevel::Info) => "[INFO] ",
        (false, LogLevel::Warn) => "[WARN] ",
        (false, LogLevel::Error) => "[ERROR] ",
    };

    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "{prefix}tactusd: {args}");
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::write_log($crate::log::LogLevel::Info, format_args!($($arg)*))
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write_log($crate::log::LogLevel::Warn, format_args!($($arg)*))
    };
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::write_log($crate::log::LogLevel::Error, format_args!($($arg)*))
    };
}

pub(crate) use {log_error, log_info, log_warn};
