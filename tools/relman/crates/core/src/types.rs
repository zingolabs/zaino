//! Value types. One module per type; invariants enforced at construction.

mod about;
mod change_entry;
mod change_kind;
mod changeset;
mod crate_name;
mod description;
mod release_options;
mod section;
mod target;
mod workspace_path;

pub use about::AboutReport;
pub use change_entry::ChangeEntry;
pub use change_kind::{ChangeKind, InvalidChangeKind};
pub use changeset::{Changeset, ChangesetError};
pub use crate_name::{CrateName, InvalidCrateName};
pub use description::{Description, EmptyDescription};
pub use release_options::ReleaseOptions;
pub use section::{InvalidSection, Section};
pub use target::Target;
pub use workspace_path::{InvalidWorkspacePath, WorkspacePath};

// Re-exported so downstream crates get a single, consistent chrono surface.
pub use chrono::{DateTime, Utc};
