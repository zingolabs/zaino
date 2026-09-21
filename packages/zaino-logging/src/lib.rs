#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

//! Entry points: [`init`] for binaries, [`try_init`] for tests.

use std::env;
use std::io::IsTerminal;
use std::sync::Once;

use time::macros::format_description;
use tracing::Level;
use tracing_subscriber::{
    EnvFilter,
    fmt::time::UtcTime,
    layer::SubscriberExt,
    util::{SubscriberInitExt, TryInitError},
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
    /// Show each event's source `file:line`. Off by default — noise for general
    /// log watching; opt in with `ZAINOLOG_LOCATION` when debugging.
    location: bool,
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

        // Source `file:line` on every line is debugging detail, not something to
        // read during normal log watching. Off unless `ZAINOLOG_LOCATION` opts in.
        let location = env::var("ZAINOLOG_LOCATION")
            .ok()
            .is_some_and(|s| matches!(s.to_lowercase().as_str(), "1" | "true" | "yes" | "on"));

        Self {
            format: LogFormat::from_env(),
            color,
            location,
            level: Level::INFO,
        }
    }
}

/// Initialise logging, configured from the environment (see the [crate
/// docs](crate)). Installs the global subscriber and the panic hook.
///
/// # Panics
///
/// Panics if a global tracing subscriber has already been set.
pub fn init() {
    try_install(LogConfig::default()).expect("global tracing subscriber already set");
    install_panic_logger();
}

/// Idempotent variant of [`init`] for tests, where several test functions may
/// each try to initialise. Does not panic if a subscriber is already set.
pub fn try_init() {
    let _ = try_install(LogConfig::default());
    install_panic_logger();
}

/// Installed at most once, regardless of how many times logging is initialized.
static PANIC_LOGGER: Once = Once::new();

/// Install the panic hook that routes panics through `tracing`. Idempotent —
/// installed at most once. See the [crate docs](crate) for the panic-at-origin
/// rationale.
///
/// The hook logs each panic as a structured `error` event under the `panic`
/// target (thread, location, message), then chains to the hook already
/// installed, so the default stderr backtrace is unchanged.
fn install_panic_logger() {
    PANIC_LOGGER.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_owned());
            let location = info
                .location()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_owned());
            let thread = std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_owned();
            tracing::error!(target: "panic", %thread, %location, %message, "panic");
            previous(info);
        }));
    });
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
    // Panics are routed through `tracing` under the `panic` target by
    // `install_panic_logger`. That target is outside the `zaino*` namespace, so
    // neither the default filter nor a typical `RUST_LOG=zaino=…` enables it — the
    // structured panic-at-origin event would be silently dropped (visible only as
    // the default hook's raw stderr backtrace, and absent from the JSON sink).
    // Ensure the `panic` target is enabled at ERROR so panic visibility is
    // structural, not contingent on the operator's filter.
    let env_filter = env_filter.add_directive(
        "panic=error"
            .parse()
            .expect("static `panic=error` directive is valid"),
    );
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
                    .pretty()
                    // Source location is opt-in (`ZAINOLOG_LOCATION`) — off for
                    // general watching, on for debugging.
                    .with_file(config.location)
                    .with_line_number(config.location),
            )
            .try_init(),
        LogFormat::Json => registry
            .with(
                // JSON format keeps full RFC3339 timestamps for machine parsing
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_timer(UtcTime::rfc_3339())
                    .with_target(true)
                    .with_file(config.location)
                    .with_line_number(config.location),
            )
            .try_init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_logger_installs_idempotently_and_still_unwinds() {
        // Idempotent (Once), and it only *adds* structured logging — a panic
        // still unwinds and is catchable, the hook does not swallow it.
        install_panic_logger();
        install_panic_logger();
        let caught = std::panic::catch_unwind(|| panic!("boom"));
        assert!(caught.is_err(), "panic still propagates through the hook");
    }

    #[test]
    fn test_log_format_from_str() {
        assert_eq!(LogFormat::parse_str("tree"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("TREE"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("stream"), Some(LogFormat::Stream));
        assert_eq!(LogFormat::parse_str("json"), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse_str("unknown"), None);
    }
}
