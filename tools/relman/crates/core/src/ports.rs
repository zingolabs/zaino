//! The seams. Two directions:
//!
//! - **driven** (outbound): traits the application *needs*, implemented by
//!   adapters (and mocks). The domain calls these.
//! - **driving** (inbound): traits the application *offers*, implemented by
//!   the domain and consumed by delivery mechanisms (CLI, and later e.g. MCP).

mod driven;
mod driving;

pub use driven::{
    ChangelogError, ChangelogStore, ChangesetStore, ChangesetStoreError, Clock, ManifestEditor,
    ManifestError, SlugSource, UidSource, Vcs, VcsError, Workspace, WorkspaceError,
};
pub use driving::{
    About, ApplyBump, ApplyError, ArtifactError, Changelog, ChangelogEdit, ChangelogGenError,
    ChangesetCheck, Changesets, ChangesetsError, CheckError, CheckReport, DeriveError,
    NewChangeset, ReleaseArtifacts, Versions, Violation,
};
