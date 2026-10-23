//! logfmt to stdout ([`tracing_logfmt`]), filtered by `RUST_LOG` (unset → zaino crates at `info`)

use tracing_subscriber::EnvFilter;
use tracing_subscriber::{filter::FromEnvError, layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_FILTER: &str = "zaino=info,zainod=info";

/// # Panics
///
/// A global tracing subscriber already set
pub(crate) fn init() -> Result<(), FromEnvError> {
    let filter = match std::env::var_os(EnvFilter::DEFAULT_ENV) {
        Some(_) => EnvFilter::try_from_default_env()?,
        None => EnvFilter::new(DEFAULT_FILTER),
    };
    let logfmt = tracing_logfmt::builder().with_span_name(false).with_span_path(false);
    let installed = tracing_subscriber::registry().with(filter).with(logfmt.layer()).try_init();
    installed.expect("global tracing subscriber already set");
    Ok(())
}

/// Root span (`component=…` on every line under it, never a chain)
///
/// - ERROR level: created under any filter that lets a zainod line through
pub(crate) fn component(name: &str) -> tracing::Span {
    tracing::error_span!(parent: None, "component", component = %name)
}
