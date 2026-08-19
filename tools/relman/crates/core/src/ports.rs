//! The seams. Two directions:
//!
//! - **driven** (outbound): traits the application *needs*, implemented by
//!   adapters (and mocks). The domain calls these.
//! - **driving** (inbound): traits the application *offers*, implemented by
//!   the domain and consumed by delivery mechanisms (CLI, and later e.g. MCP).

mod driven;
mod driving;

pub use driven::{ChangesetStore, ChangesetStoreError, Clock, SlugSource, Vcs, VcsError};
pub use driving::{
    About, ChangesetCheck, Changesets, ChangesetsError, CheckError, CheckReport, NewChangeset,
    Violation,
};
