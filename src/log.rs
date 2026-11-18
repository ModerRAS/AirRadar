use std::sync::OnceLock;

use chrono::Utc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn as_str(&self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }

    fn parse(value: &str) -> Level {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "debug" | "trace" => Level::Debug,
            _ => Level::Info,
        }
    }
}

static LEVEL: OnceLock<Level> = OnceLock::new();

/// Initialise global log level from a string (LOG_LEVEL).
pub fn init(level: &str) {
    let _ = LEVEL.set(Level::parse(level));
}

/// Initialise from the LOG_LEVEL environment variable, defaulting to info.
pub fn init_from_env() {
    let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    init(&level);
}

fn enabled(level: Level) -> bool {
    level <= *LEVEL.get().unwrap_or(&Level::Info)
}

fn emit(level: Level, message: &str) {
    if enabled(level) {
        println!(
            "{} {:<5} {}",
            Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            level.as_str(),
            message
        );
    }
}

pub fn error(message: impl AsRef<str>) {
    emit(Level::Error, message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    emit(Level::Warn, message.as_ref());
}

pub fn info(message: impl AsRef<str>) {
    emit(Level::Info, message.as_ref());
}

pub fn debug(message: impl AsRef<str>) {
    emit(Level::Debug, message.as_ref());
}
