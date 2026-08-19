//! Adapters: concrete driven-port implementations.
//!
//! The outer ring of the hexagon. Each type here implements a `relman-core`
//! driven port against a real resource (the filesystem, a random generator).
//! Only the binary's composition root names these; the domain depends on the
//! port traits, never on this crate.

mod cargo_metadata_workspace;
mod fs_changeset_store;
mod git_vcs;
mod random_slug_source;

pub use cargo_metadata_workspace::CargoMetadataWorkspace;
pub use fs_changeset_store::FsChangesetStore;
pub use git_vcs::GitVcs;
pub use random_slug_source::RandomSlugSource;
