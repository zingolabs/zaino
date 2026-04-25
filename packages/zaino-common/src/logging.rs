//! Logging infrastructure for Zaino.
//!
//! This module provides centralized logging configuration with support for:
//! - Stream view (flat chronological output) - DEFAULT
//! - Tree view (hierarchical span-based output)
//! - JSON output (machine-parseable)
//!
//! # Environment Variables
//!
//! - `RUST_LOG`: Standard tracing filter. By default only zaino crates are logged.
//!   Set `RUST_LOG=info` to include all crates (zebra, etc.), or use specific
//!   filters like `RUST_LOG=zaino=debug,zebra_state=info`.
//! - `ZAINOLOG_FORMAT`: Output format ("stream", "tree", or "json")
//! - `ZAINOLOG_COLOR`: Color mode ("true"/"false"/"auto"). Defaults to color enabled.
//!
//! # Example
//!
//! ```no_run
//! use zaino_common::logging;
//!
//! // Initialize with defaults (tree format, info level)
//! logging::init();
//!
//! // Or with custom configuration
//! logging::init_with_config(logging::LogConfig::default().format(logging::LogFormat::Json));
//! ```

use std::env;
use std::fmt;
use std::io::IsTerminal;

use time::macros::format_description;
use tracing::Level;
use tracing_subscriber::{
    fmt::{format::FmtSpan, time::UtcTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};
use tracing_tree::HierarchicalLayer;

/// Time format for logs: HH:MM:SS.subsec (compact, no date)
const TIME_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Hierarchical tree view showing span nesting.
    Tree,
    /// Flat chronological stream (default).
    #[default]
    Stream,
    /// Machine-parseable JSON.
    Json,
}

impl LogFormat {
    /// Parse from string (case-insensitive).
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "tree" => Some(LogFormat::Tree),
            "stream" => Some(LogFormat::Stream),
            "json" => Some(LogFormat::Json),
            _ => None,
        }
    }

    /// Get from ZAINOLOG_FORMAT environment variable.
    pub fn from_env() -> Self {
        env::var("ZAINOLOG_FORMAT")
            .ok()
            .and_then(|s| Self::parse_str(&s))
            .unwrap_or_default()
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogFormat::Tree => write!(f, "tree"),
            LogFormat::Stream => write!(f, "stream"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Output format (tree, stream, or json).
    pub format: LogFormat,
    /// Enable ANSI colors.
    pub color: bool,
    /// Default log level.
    pub level: Level,
    /// Show span events (enter/exit).
    pub show_span_events: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        // Check ZAINOLOG_COLOR env var:
        // - "true"/"1"/etc: force color on
        // - "false"/"0"/etc: force color off
        // - "auto": auto-detect TTY (default behavior)
        // If not set, default to color enabled (better dev experience)
        let color = env::var("ZAINOLOG_COLOR")
            .ok()
            .and_then(|s| match s.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                "auto" => Some(std::io::stderr().is_terminal()),
                _ => None,
            })
            .unwrap_or(true); // Default to color enabled

        Self {
            format: LogFormat::from_env(),
            color,
            level: Level::INFO,
            show_span_events: false,
        }
    }
}

impl LogConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the log format.
    pub fn format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Enable or disable colors.
    pub fn color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Set the default log level.
    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Show span enter/exit events.
    pub fn show_span_events(mut self, show: bool) -> Self {
        self.show_span_events = show;
        self
    }
}

/// Initialize logging with default configuration.
///
/// Reads `ZAINOLOG_FORMAT` environment variable to determine format:
/// - "stream" (default): Flat chronological output with timestamps
/// - "tree": Hierarchical span-based output
/// - "json": Machine-parseable JSON
pub fn init() {
    init_with_config(LogConfig::default());
}

/// Initialize logging with custom configuration.
pub fn init_with_config(config: LogConfig) {
    // If RUST_LOG is set, use it directly. Otherwise, default to zaino crates only.
    // Users can set RUST_LOG=info to see all crates including zebra.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "zaino={level},zainod={level},zainodlib={level}",
            level = config.level.as_str()
        ))
    });

    match config.format {
        LogFormat::Tree => init_tree(env_filter, config),
        LogFormat::Stream => init_stream(env_filter, config),
        LogFormat::Json => init_json(env_filter),
    }
}

/// Try to initialize logging (won't fail if already initialized).
///
/// Useful for tests where multiple test functions may try to initialize logging.
pub fn try_init() {
    try_init_with_config(LogConfig::default());
}

/// Try to initialize logging with custom configuration.
pub fn try_init_with_config(config: LogConfig) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "zaino={level},zainod={level},zainodlib={level}",
            level = config.level.as_str()
        ))
    });

    match config.format {
        LogFormat::Tree => {
            let _ = try_init_tree(env_filter, config);
        }
        LogFormat::Stream => {
            let _ = try_init_stream(env_filter, config);
        }
        LogFormat::Json => {
            let _ = try_init_json(env_filter);
        }
    }
}

fn init_tree(env_filter: EnvFilter, config: LogConfig) {
    let layer = HierarchicalLayer::new(2)
        .with_ansi(config.color)
        .with_targets(true)
        .with_bracketed_fields(true)
        .with_indent_lines(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_deferred_spans(true) // Only show spans when they have events
        .with_verbose_entry(false) // Don't repeat span info on entry
        .with_verbose_exit(false); // Don't repeat span info on exit

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layer)
        .init();
}

fn try_init_tree(
    env_filter: EnvFilter,
    config: LogConfig,
) -> Result<(), tracing_subscriber::util::TryInitError> {
    let layer = HierarchicalLayer::new(2)
        .with_ansi(config.color)
        .with_targets(true)
        .with_bracketed_fields(true)
        .with_indent_lines(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_deferred_spans(true) // Only show spans when they have events
        .with_verbose_entry(false) // Don't repeat span info on entry
        .with_verbose_exit(false); // Don't repeat span info on exit

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layer)
        .try_init()
}

fn init_stream(env_filter: EnvFilter, config: LogConfig) {
    let span_events = if config.show_span_events {
        FmtSpan::ENTER | FmtSpan::EXIT
    } else {
        FmtSpan::NONE
    };

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(UtcTime::new(TIME_FORMAT))
        .with_target(true)
        .with_ansi(config.color)
        .with_span_events(span_events)
        .pretty()
        .init();
}

fn try_init_stream(
    env_filter: EnvFilter,
    config: LogConfig,
) -> Result<(), tracing_subscriber::util::TryInitError> {
    let span_events = if config.show_span_events {
        FmtSpan::ENTER | FmtSpan::EXIT
    } else {
        FmtSpan::NONE
    };

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_timer(UtcTime::new(TIME_FORMAT))
        .with_target(true)
        .with_ansi(config.color)
        .with_span_events(span_events)
        .pretty();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
}

fn init_json(env_filter: EnvFilter) {
    // JSON format keeps full RFC3339 timestamps for machine parsing
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .with_timer(UtcTime::rfc_3339())
        .with_target(true)
        .init();
}

fn try_init_json(env_filter: EnvFilter) -> Result<(), tracing_subscriber::util::TryInitError> {
    // JSON format keeps full RFC3339 timestamps for machine parsing
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_timer(UtcTime::rfc_3339())
        .with_target(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
}

/// Wrapper for displaying hashes in a compact format.
///
/// Shows the first 4 bytes (8 hex chars) followed by "..".
pub struct DisplayHash<'a>(pub &'a [u8]);

impl fmt::Display for DisplayHash<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.len() >= 4 {
            write!(f, "{}..", hex::encode(&self.0[..4]))
        } else {
            write!(f, "{}", hex::encode(self.0))
        }
    }
}

/// Wrapper for displaying hex strings in a compact format.
///
/// Shows the first 8 chars followed by "..".
pub struct DisplayHexStr<'a>(pub &'a str);

impl fmt::Display for DisplayHexStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.len() > 8 {
            write!(f, "{}..", &self.0[..8])
        } else {
            write!(f, "{}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_format_from_str() {
        assert_eq!(LogFormat::parse_str("tree"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("TREE"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("stream"), Some(LogFormat::Stream));
        assert_eq!(LogFormat::parse_str("json"), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse_str("unknown"), None);
    }

    #[test]
    fn test_display_hash() {
        let hash = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        assert_eq!(format!("{}", DisplayHash(&hash)), "12345678..");
    }

    #[test]
    fn test_display_hex_str() {
        let hex_str = "1234567890abcdef";
        assert_eq!(format!("{}", DisplayHexStr(hex_str)), "12345678..");

        let short_hex = "1234";
        assert_eq!(format!("{}", DisplayHexStr(short_hex)), "1234");
    }

    #[test]
    fn test_config_builder() {
        let config = LogConfig::new()
            .format(LogFormat::Json)
            .color(false)
            .level(Level::DEBUG);

        assert_eq!(config.format, LogFormat::Json);
        assert!(!config.color);
        assert_eq!(config.level, Level::DEBUG);
    }
}
