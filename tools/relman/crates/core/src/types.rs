//! Value types. One module per type; invariants enforced at construction.

mod about;
mod bump;
mod bump_table;
mod change_entry;
mod change_kind;
mod changeset;
mod consumed_ledger;
mod crate_name;
mod cycle_id;
mod cycle_status;
mod description;
mod publish_plan;
mod release_options;
mod section;
mod slug;
mod tag;
mod tag_plan;
mod target;
mod uid;
mod version;
mod workspace_path;

pub use about::AboutReport;
pub use bump::Bump;
pub use bump_table::{BumpTable, CrateBump};
pub use change_entry::ChangeEntry;
pub use change_kind::{ChangeKind, InvalidChangeKind};
pub use changeset::{CONSUMED_IN_KEY, Changeset, ChangesetError, StoredChangeset};
pub use consumed_ledger::{ConsumedEntry, ConsumedLedger, ConsumedLedgerError};
pub use crate_name::{CrateName, InvalidCrateName};
pub use cycle_id::{CycleId, InvalidCycleId};
pub use cycle_status::{
    Commit, CycleStatus, CycleStatusError, DeploymentStatus, InvalidCommit,
    InvalidDeploymentStatus, RcEntry, Watermarks,
};
pub use description::{Description, EmptyDescription};
pub use publish_plan::PublishPlan;
pub use release_options::ReleaseOptions;
pub use section::{InvalidSection, Section};
pub use slug::{InvalidSlug, Slug};
pub use tag::{InvalidTag, Tag};
pub use tag_plan::TagPlan;
pub use target::Target;
pub use uid::{InvalidUid, Uid};
pub use version::{InvalidVersion, Version};
pub use workspace_path::{InvalidWorkspacePath, WorkspacePath};

// Re-exported so downstream crates get a single, consistent chrono surface.
pub use chrono::{DateTime, NaiveDate, Utc};
