//! Test support for this crate, and the chain vectors its suites run against.
//!
//! # Available to dependants, behind `testing`
//!
//! [`vectors`] is compiled into a normal build when the `testing` feature is
//! on, so `zaino-state`'s remaining suites can read the same chain rather than
//! keeping a second copy of it. That feature is dev-dependency-only —
//! `resolver = "2"` keeps dev-dependency features out of production graphs — so
//! none of this reaches a shipped build.
//!
//! [`fake_validator`] stays `cfg(test)`: it exists to answer this crate's four
//! source questions and nothing else, and a consumer wanting a mock validator
//! wants a different one.

#[cfg(test)]
pub(crate) mod fake_validator;

#[cfg(test)]
pub(crate) mod finalised_state;

#[cfg(any(test, feature = "testing"))]
pub mod fixtures;

#[cfg(any(test, feature = "testing"))]
pub mod vectors;

/// Installs a tracing subscriber, once per process.
///
/// `try_init` without unwrapping: under `cargo test` every test in a binary
/// shares one process, so the second call would fail and a panicking helper
/// would fail every test after the first. `cargo nextest` gives each test its
/// own process and would not notice, which is exactly why this must not depend
/// on which runner is used.
#[cfg(test)]
pub(crate) fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .try_init();
}
