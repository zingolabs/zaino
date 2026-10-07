//! Tracing subscriber + panic-at-origin hook, configured from the environment
//!
//! - `RUST_LOG` (unset → zaino crates at `info`), `ZAINOLOG_FORMAT` = terminal / json
//! - `ZAINOLOG_COLOR` = bool / auto (default auto), `ZAINOLOG_LOCATION` = bool (default off)
//! - Unrecognised value = startup error (a typo never silently picks another format)

mod terminal;

use std::borrow::Cow;
use std::env;
use std::fmt;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Once;

use tracing_subscriber::{
    fmt::time::UtcTime, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter,
};

/// Operator filter when `RUST_LOG` is unset (`zaino` prefix = every `zaino_*` crate)
const DEFAULT_FILTER: &str = "zaino=info,zainod=info,zainodlib=info";

#[derive(Debug, thiserror::Error)]
pub enum LogConfigError {
    #[error("{var}={value}: expected one of {expected}")]
    Unrecognised { var: &'static str, value: String, expected: &'static str },
    #[error("RUST_LOG: {0}")]
    Filter(#[from] tracing_subscriber::filter::FromEnvError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Terminal,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogConfig {
    format: LogFormat,
    color: bool,
    location: bool,
}

impl LogConfig {
    fn from_env() -> Result<Self, LogConfigError> {
        let read = |var: &'static str| env::var(var).ok().map(|v| (var, v.to_lowercase()));
        Self::parse(
            read("ZAINOLOG_FORMAT"),
            read("ZAINOLOG_COLOR"),
            read("ZAINOLOG_LOCATION"),
            std::io::stdout().is_terminal(),
        )
    }

    /// `(var, lowercased value)` per knob; `tty` = what `auto` colour resolves to
    fn parse(
        format: Option<(&'static str, String)>,
        color: Option<(&'static str, String)>,
        location: Option<(&'static str, String)>,
        tty: bool,
    ) -> Result<Self, LogConfigError> {
        let unrecognised = |(var, value): (&'static str, String), expected| {
            LogConfigError::Unrecognised { var, value, expected }
        };
        let flag = |setting: (&'static str, String)| match setting.1.as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(setting),
        };

        let format = match format {
            None => LogFormat::Terminal,
            Some(setting) => match setting.1.as_str() {
                "terminal" => LogFormat::Terminal,
                "json" => LogFormat::Json,
                _ => return Err(unrecognised(setting, "terminal/json")),
            },
        };
        let color = match color {
            None => tty,
            Some(setting) if setting.1 == "auto" => tty,
            Some(setting) => flag(setting).map_err(|s| unrecognised(s, "true/false/auto"))?,
        };
        let location = match location {
            None => false,
            Some(setting) => flag(setting).map_err(|s| unrecognised(s, "true/false"))?,
        };
        Ok(Self { format, color, location })
    }
}

/// Global subscriber + panic hook
///
/// # Panics
///
/// A global tracing subscriber already set
pub fn init() -> Result<(), LogConfigError> {
    install(LogConfig::from_env()?, true)?;
    install_panic_logger();
    Ok(())
}

/// Idempotent [`init`] (library entry → the binary may already have installed one)
pub(crate) fn try_init() -> Result<(), LogConfigError> {
    install(LogConfig::from_env()?, false)?;
    install_panic_logger();
    Ok(())
}

static PANIC_LOGGER: Once = Once::new();

/// Panic → structured `error` on target `panic`, then the previous hook (backtrace unchanged)
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
            let thread = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
            tracing::error!(target: "panic", %thread, %location, %message, "Panic");
            previous(info);
        }));
    });
}

/// Span owning every line under it: the terminal's component column, a span field in JSON
///
/// - ERROR level: created under any filter that lets a zainod line through
pub(crate) fn component(name: &str) -> tracing::Span {
    tracing::error_span!("component", component = name)
}

/// [`component`] named for an index: its `NAME` `compact_block` → `CompactBlockIdx`,
/// `transparent_address` → `TransparentAddrIdx`
pub(crate) fn index_component(name: &str) -> tracing::Span {
    let camel: String = name
        .split('_')
        .map(|word| match word {
            "address" => "addr",
            word => word,
        })
        .flat_map(|word| {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_ascii_uppercase()).into_iter().chain(chars)
        })
        .collect();
    component(&format!("{camel}Idx"))
}

/// Height column: grouped (`3,501,802`), `—` when absent (terminal pads it to the column)
pub(crate) struct HeightCol(pub(crate) Option<u32>);

impl fmt::Display for HeightCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(height) => f.write_str(&terminal::grouped(u64::from(height))),
            None => f.write_str("—"),
        }
    }
}

/// Paths in a message: past this many columns, the head elided (the tail names the index)
const PATH_WIDTH: usize = 48;

pub(crate) fn shown_path(path: &Path) -> Cow<'_, str> {
    match path.to_string_lossy() {
        Cow::Borrowed(text) => unicode_ellipsis::truncate_str_leading(text, PATH_WIDTH),
        Cow::Owned(text) => {
            Cow::Owned(unicode_ellipsis::truncate_str_leading(&text, PATH_WIDTH).into_owned())
        }
    }
}

/// `exclusive` → a subscriber already set is a bug (the binary); else it wins (a library caller)
fn install(config: LogConfig, exclusive: bool) -> Result<(), LogConfigError> {
    let filter = match env::var_os(EnvFilter::DEFAULT_ENV) {
        Some(_) => EnvFilter::try_from_default_env()?,
        None => EnvFilter::new(DEFAULT_FILTER),
    };
    // `panic` target outside `zaino*` → forced on (else any zaino-only filter drops it)
    let filter = filter
        .add_directive("panic=error".parse().expect("static `panic=error` directive is valid"));
    let registry = tracing_subscriber::registry().with(filter);

    let installed = match config.format {
        LogFormat::Terminal => registry
            .with(terminal::Components)
            .with(tracing_subscriber::fmt::layer().fmt_fields(terminal::Logfmt).event_format(
                terminal::Terminal { color: config.color, location: config.location },
            ))
            .try_init(),
        LogFormat::Json => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_timer(UtcTime::rfc_3339())
                    .with_target(true)
                    .with_file(config.location)
                    .with_line_number(config.location),
            )
            .try_init(),
    };
    if exclusive {
        installed.expect("global tracing subscriber already set");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_logger_installs_idempotently_and_still_unwinds() {
        install_panic_logger();
        install_panic_logger();
        let caught = std::panic::catch_unwind(|| panic!("boom"));
        assert!(caught.is_err(), "panic still propagates through the hook");
    }

    /// Defaults, every accepted spelling, and an unrecognised value per knob refused by name
    #[test]
    fn config_parses_every_knob_and_refuses_what_it_does_not_know() {
        let set = |var: &'static str, value: &str| Some((var, value.to_owned()));
        let parse = |format, color, location, tty| LogConfig::parse(format, color, location, tty);
        let config = |format, color, location| LogConfig { format, color, location };

        use LogConfigError::Unrecognised;
        let (terminal, json) = (LogFormat::Terminal, LogFormat::Json);
        let explicit = parse(
            set("ZAINOLOG_FORMAT", "json"),
            set("ZAINOLOG_COLOR", "auto"),
            set("ZAINOLOG_LOCATION", "on"),
            true,
        );

        assert_eq!(parse(None, None, None, true).expect("defaults"), config(terminal, true, false));
        assert_eq!(parse(None, None, None, false).expect("piped"), config(terminal, false, false));
        assert_eq!(explicit.expect("explicit"), config(json, true, true));
        let forced_off = parse(None, set("ZAINOLOG_COLOR", "0"), None, true);
        assert_eq!(forced_off.expect("forced off"), config(terminal, false, false));

        for (format, color, location, var) in [
            (set("ZAINOLOG_FORMAT", "stream"), None, None, "ZAINOLOG_FORMAT"),
            (None, set("ZAINOLOG_COLOR", "maybe"), None, "ZAINOLOG_COLOR"),
            (None, None, set("ZAINOLOG_LOCATION", "auto"), "ZAINOLOG_LOCATION"),
        ] {
            let refused = parse(format, color, location, true);
            assert!(matches!(refused, Err(Unrecognised { var: v, .. }) if v == var), "{var}");
        }
    }
}
