//! In-memory port implementations and fixtures for tests.
//!
//! Behind the `test-support` feature so it never ships in release builds.
//! Downstream crates enable it as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! relman-core = { workspace = true, features = ["test-support"] }
//! ```

mod changelog_store;
mod changeset_store;
mod clock;
pub mod fixtures;
mod manifest_editor;
mod slug_source;
mod vcs;
mod workspace;

pub use changelog_store::MapChangelogStore;
pub use changeset_store::MapChangesetStore;
pub use clock::FixedClock;
pub use manifest_editor::RecordingManifestEditor;
pub use slug_source::SequenceSlugSource;
pub use vcs::StubVcs;
pub use workspace::MapWorkspace;
