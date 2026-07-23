//! `zaino-runtime` — the orchestrator.
//!
//! Composes the finalised-state (`zaino-fs`) and non-finalised-state
//! (`zaino-nfs`) components into one running indexer and implements
//! `zaino-service::IndexerService`.
//!
//! Module layout:
//! - [`runtime`] — the **supervisor**: owns the components, runs their loops,
//!   aggregates serviceability, holds config, produces the read-context. No
//!   query logic.
//! - [`snapshot`] — the pinned **read-context** and its capability impls; thin,
//!   they delegate the composition decision to [`resolve`].
//! - [`resolve`] — the **read-composition policy**: route / merge / passthrough,
//!   and the passthrough decision, in one place.
//! - [`config`] / [`error`].
#![forbid(unsafe_code)]

mod config;
mod error;
mod passthrough;
mod resolve;
mod runtime;
mod snapshot;

pub use config::RuntimeConfig;
pub use error::RuntimeError;
pub use passthrough::{PassthroughError, PassthroughSource};
pub use runtime::{Runtime, RuntimeBuilder};
pub use snapshot::RuntimeSnapshot;
