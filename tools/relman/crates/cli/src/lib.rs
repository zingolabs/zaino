//! CLI: the delivery adapter.
//!
//! Parses arguments (clap) and calls the application through driving ports
//! held in [`Ctx`]. It knows nothing of concrete services or adapters — the
//! binary builds [`Ctx`] and hands it in.

mod app;
pub mod commands;
mod context;
mod format;

pub use app::{Cli, Command};
pub use commands::bump::BumpCommandError;
pub use commands::changeset::ChangesetCommandError;
pub use commands::versions::VersionsCommandError;
pub use context::Ctx;
