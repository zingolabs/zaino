//! Value types. One module per type; invariants enforced at construction.

mod about;
mod crate_name;
mod release_options;
mod target;
mod workspace_path;

pub use about::AboutReport;
pub use crate_name::{CrateName, InvalidCrateName};
pub use release_options::ReleaseOptions;
pub use target::Target;
pub use workspace_path::{InvalidWorkspacePath, WorkspacePath};

// Re-exported so downstream crates get a single, consistent chrono surface.
pub use chrono::{DateTime, Utc};
