//! Config adapter (slice-0 placeholder).
//!
//! Later slices parse the repo-committed `relman.toml` — the authority for
//! relman's governed versioning targets and options — into typed core
//! newtypes at the composition-root boundary (serde + toml, both already
//! declared in the workspace). For now this crate carries only a placeholder
//! [`ReleaseConfig`] so the topology compiles; no parsing is implemented yet.

mod config;

pub use config::ReleaseConfig;
