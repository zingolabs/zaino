//! Stock `tracing_subscriber::fmt` subscriber + panic-at-origin hook, configured from the env
//!
//! - `RUST_LOG` (unset → zaino crates at `info`), `ZAINOLOG_FORMAT` = terminal / json
//! - `ZAINOLOG_COLOR` = bool / auto (default auto), `ZAINOLOG_LOCATION` = bool (default off)
//! - Unrecognised value = startup error (a typo never silently picks another format)

use std::env;
use std::io::IsTerminal;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use zaino_persistence::IndexKind;

/// Operator filter when `RUST_LOG` is unset (`zaino` prefix = every `zaino_*` crate)
const DEFAULT_FILTER: &str = "zaino=info,zainod=info";

#[derive(Debug, thiserror::Error)]
pub(crate) enum LogConfigError {
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
        Self::parse(|var| env::var(var).ok(), std::io::stdout().is_terminal())
    }

    /// `env` = a variable lookup, `tty` = what `auto` colour resolves to
    fn parse(env: impl Fn(&str) -> Option<String>, tty: bool) -> Result<Self, LogConfigError> {
        let read = |var| env(var).map(|value| value.to_lowercase());
        let format = match read("ZAINOLOG_FORMAT").as_deref() {
            None | Some("terminal") => LogFormat::Terminal,
            Some("json") => LogFormat::Json,
            Some(value) => return Err(unrecognised("ZAINOLOG_FORMAT", value, "terminal/json")),
        };
        let color = match read("ZAINOLOG_COLOR").as_deref() {
            None | Some("auto") => tty,
            Some(value) => flag(value)
                .ok_or_else(|| unrecognised("ZAINOLOG_COLOR", value, "true/false/auto"))?,
        };
        let location = match read("ZAINOLOG_LOCATION").as_deref() {
            None => false,
            Some(value) => {
                flag(value).ok_or_else(|| unrecognised("ZAINOLOG_LOCATION", value, "true/false"))?
            }
        };
        Ok(Self { format, color, location })
    }
}

fn flag(value: &str) -> Option<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn unrecognised(var: &'static str, value: &str, expected: &'static str) -> LogConfigError {
    LogConfigError::Unrecognised { var, value: value.to_owned(), expected }
}

/// Global subscriber + panic hook
///
/// # Panics
///
/// A global tracing subscriber already set
pub(crate) fn init() -> Result<(), LogConfigError> {
    install(LogConfig::from_env()?)?;
    install_panic_logger();
    Ok(())
}

/// Panic → structured `error` on target `panic`, then the previous hook (backtrace unchanged)
fn install_panic_logger() {
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
}

/// Span owning every line under it (`component{component=ZainoNFS}:`)
///
/// - root (a component opened inside another = one prefix per line, not the chain)
/// - ERROR level: created under any filter that lets a zainod line through
pub(crate) fn component(name: &str) -> tracing::Span {
    tracing::error_span!(parent: None, "component", component = %name)
}

pub(crate) fn index_component(kind: IndexKind) -> tracing::Span {
    component(match kind {
        IndexKind::CompactBlock => "CompactBlockIdx",
        IndexKind::UtxoSet => "UtxoSetIdx",
        IndexKind::BlockHash => "BlockHashIdx",
        IndexKind::TreeState => "TreeStateIdx",
        IndexKind::TransparentAddress => "TransparentAddrIdx",
        IndexKind::TxidLocation => "TxidLocationIdx",
    })
}

fn install(config: LogConfig) -> Result<(), LogConfigError> {
    let filter = match env::var_os(EnvFilter::DEFAULT_ENV) {
        Some(_) => EnvFilter::try_from_default_env()?,
        None => EnvFilter::new(DEFAULT_FILTER),
    };
    // `panic` target outside `zaino*` → forced on (else any zaino-only filter drops it)
    let filter = filter
        .add_directive("panic=error".parse().expect("static `panic=error` directive is valid"));
    let layer = tracing_subscriber::fmt::layer()
        .with_file(config.location)
        .with_line_number(config.location);
    let registry = tracing_subscriber::registry().with(filter);
    let installed = match config.format {
        LogFormat::Terminal => registry.with(layer.with_ansi(config.color)).try_init(),
        LogFormat::Json => registry.with(layer.json()).try_init(),
    };
    installed.expect("global tracing subscriber already set");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults, every accepted spelling, and an unrecognised value per knob refused by name
    #[test]
    fn config_parses_every_knob_and_refuses_what_it_does_not_know() {
        let parse = |vars: &[(&str, &str)], tty| {
            let env =
                |var: &str| vars.iter().find(|(name, _)| *name == var).map(|(_, v)| v.to_string());
            LogConfig::parse(env, tty)
        };
        let config = |format, color, location| LogConfig { format, color, location };

        use LogConfigError::Unrecognised;
        let (terminal, json) = (LogFormat::Terminal, LogFormat::Json);
        let explicit =
            [("ZAINOLOG_FORMAT", "JSON"), ("ZAINOLOG_COLOR", "auto"), ("ZAINOLOG_LOCATION", "on")];

        assert_eq!(parse(&[], true).expect("defaults"), config(terminal, true, false));
        assert_eq!(parse(&[], false).expect("piped"), config(terminal, false, false));
        assert_eq!(parse(&explicit, true).expect("explicit"), config(json, true, true));
        let forced_off = parse(&[("ZAINOLOG_COLOR", "0")], true);
        assert_eq!(forced_off.expect("forced off"), config(terminal, false, false));

        for (var, value) in [
            ("ZAINOLOG_FORMAT", "stream"),
            ("ZAINOLOG_COLOR", "maybe"),
            ("ZAINOLOG_LOCATION", "auto"),
        ] {
            let refused = parse(&[(var, value)], true);
            assert!(matches!(refused, Err(Unrecognised { var: v, .. }) if v == var), "{var}");
        }
    }
}
