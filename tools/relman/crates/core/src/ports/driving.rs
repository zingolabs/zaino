use crate::ports::ChangesetStoreError;
use crate::types::{AboutReport, Description, Slug};

/// Inbound port: report who relman is (version) and what it thinks "now" is.
///
/// Implemented by the domain (`AboutService`) and consumed by delivery
/// mechanisms through `Arc<dyn About>`. Callers never name the concrete
/// service — only the binary's composition root does. This is the trivial
/// live thread that keeps every seam exercised; the real driving ports
/// (`Changesets`, `Versions`, `Bump`, `Changelog`, `ReleaseArtifacts`) arrive
/// in later slices.
pub trait About: Send + Sync {
    fn report(&self) -> AboutReport;
}

/// What kind of changeset to scaffold with `relman changeset new`.
///
/// Mirrors the two authoring shapes from the changeset-format decision record:
/// a to-be-edited template, or the `[empty]` escape hatch with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewChangeset {
    /// A commented scaffold the author edits into real `[[changes]]` entries.
    Template,
    /// The no-op `[empty]` form, carrying its required justification.
    Empty {
        /// Why this PR is release-irrelevant.
        reason: Description,
    },
}

/// Everything that can go wrong creating a new changeset.
#[derive(Debug, thiserror::Error)]
pub enum ChangesetsError {
    /// A store operation failed.
    #[error("changeset store operation failed")]
    Store(#[from] ChangesetStoreError),
    /// Every candidate slug collided with an existing file within the retry
    /// budget — vanishingly unlikely, so it signals an exhausted or degenerate
    /// slug source rather than ordinary contention.
    #[error("could not find a unique changeset slug after {tries} attempts")]
    NoUniqueSlug {
        /// How many candidates were tried before giving up.
        tries: usize,
    },
}

/// Inbound port: author new changeset files.
///
/// Implemented by the domain (`ChangesetService`) over the [`ChangesetStore`]
/// and [`SlugSource`] driven ports. Picks a unique slug, renders the requested
/// shape, writes it, and returns the chosen [`Slug`].
///
/// [`ChangesetStore`]: crate::ports::ChangesetStore
/// [`SlugSource`]: crate::ports::SlugSource
pub trait Changesets: Send + Sync {
    /// Create a new changeset file, returning the slug it was written under.
    ///
    /// Named `new` to mirror the `relman changeset new` command it backs, not a
    /// constructor — hence the lint allow.
    #[allow(clippy::new_ret_no_self, clippy::wrong_self_convention)]
    fn new(&self, req: NewChangeset) -> Result<Slug, ChangesetsError>;
}
