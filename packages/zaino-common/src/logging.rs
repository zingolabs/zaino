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
//! // Initialize logging, configured via the environment variables above.
//! logging::init();
//! ```

use std::env;
use std::io::IsTerminal;

use time::macros::format_description;
use tracing::Level;
use tracing_subscriber::{
    fmt::time::UtcTime,
    layer::SubscriberExt,
    util::{SubscriberInitExt, TryInitError},
    EnvFilter,
};
use tracing_tree::HierarchicalLayer;

/// Time format for logs: HH:MM:SS.subsec (compact, no date)
const TIME_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LogFormat {
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
    fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "tree" => Some(LogFormat::Tree),
            "stream" => Some(LogFormat::Stream),
            "json" => Some(LogFormat::Json),
            _ => None,
        }
    }

    /// Get from ZAINOLOG_FORMAT environment variable.
    fn from_env() -> Self {
        env::var("ZAINOLOG_FORMAT")
            .ok()
            .and_then(|s| Self::parse_str(&s))
            .unwrap_or_default()
    }
}

/// Logging configuration, read from the environment.
#[derive(Debug, Clone)]
struct LogConfig {
    /// Output format (tree, stream, or json).
    format: LogFormat,
    /// Enable ANSI colors.
    color: bool,
    /// Default log level.
    level: Level,
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
        }
    }
}

/// Initialize logging, configured from the environment (see module docs).
///
/// # Panics
///
/// Panics if a global tracing subscriber has already been set.
pub fn init() {
    try_install(LogConfig::default()).expect("global tracing subscriber already set");
}

/// Try to initialize logging (won't fail if already initialized).
///
/// Useful for tests where multiple test functions may try to initialize logging.
pub fn try_init() {
    let _ = try_install(LogConfig::default());
}

/// Build the subscriber described by `config` and install it as the global
/// default, erroring if one is already set.
fn try_install(config: LogConfig) -> Result<(), TryInitError> {
    // If RUST_LOG is set, use it directly. Otherwise, default to zaino crates only.
    // Users can set RUST_LOG=info to see all crates including zebra.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "zaino={level},zainod={level},zainodlib={level}",
            level = config.level.as_str()
        ))
    });
    let registry = tracing_subscriber::registry().with(env_filter);

    match config.format {
        LogFormat::Tree => registry
            .with(
                HierarchicalLayer::new(2)
                    .with_ansi(config.color)
                    .with_targets(true)
                    .with_bracketed_fields(true)
                    .with_indent_lines(true)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .with_deferred_spans(true) // Only show spans when they have events
                    .with_verbose_entry(false) // Don't repeat span info on entry
                    .with_verbose_exit(false), // Don't repeat span info on exit
            )
            .try_init(),
        LogFormat::Stream => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(UtcTime::new(TIME_FORMAT))
                    .with_target(true)
                    .with_ansi(config.color)
                    .pretty(),
            )
            .try_init(),
        LogFormat::Json => registry
            .with(
                // JSON format keeps full RFC3339 timestamps for machine parsing
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_timer(UtcTime::rfc_3339())
                    .with_target(true),
            )
            .try_init(),
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
}
