use std::path::PathBuf;

use crate::ports::{ChangesetStoreError, VcsError};
use crate::types::{AboutReport, CrateName, Description, Slug};

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

/// A single reason a PR fails changeset enforcement.
///
/// A violation is the *expected* "this PR is non-compliant" signal, not an
/// error: [`ChangesetCheck::check`] returns them in a [`CheckReport`] on the
/// `Ok` path. Only infrastructure failures (VCS, store) become a [`CheckError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A target whose source changed is covered by no changeset entry in this
    /// PR.
    TargetUncovered(CrateName),
    /// The PR changed governed-crate source but added no changeset file at all.
    NoChangesetForTouchedTargets,
    /// A changeset entry named a `crate` that is not a declared target.
    UnknownTargetInChangeset(String),
    /// A this-PR changeset file could not be parsed.
    ChangesetParse {
        /// The offending changeset's repo-relative path.
        file: PathBuf,
        /// The rendered parse error.
        error: String,
    },
}

impl Violation {
    /// A human-readable, single-line diagnostic for this violation.
    pub fn message(&self) -> String {
        match self {
            Violation::TargetUncovered(name) => {
                format!("target `{name}` has changed source but no changeset entry covers it")
            }
            Violation::NoChangesetForTouchedTargets => {
                "this PR changes governed-crate source but adds no changeset file".to_owned()
            }
            Violation::UnknownTargetInChangeset(name) => {
                format!("changeset names `{name}`, which is not a declared target")
            }
            Violation::ChangesetParse { file, error } => {
                format!("failed to parse changeset `{}`: {error}", file.display())
            }
        }
    }
}

/// The outcome of a changeset check: the (possibly empty) set of violations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckReport {
    /// Every reason the PR is non-compliant. Empty means the PR passes.
    pub violations: Vec<Violation>,
}

impl CheckReport {
    /// Whether the PR is compliant (no violations).
    pub fn is_ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// An infrastructure failure that prevented the check from running to a verdict.
///
/// Distinct from a [`Violation`]: a `CheckError` means we *could not decide*
/// (the VCS query or the store read failed), not that the PR is non-compliant.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// Querying version control failed.
    #[error("version-control query failed")]
    Vcs(#[from] VcsError),
    /// A changeset-store operation failed.
    #[error("changeset store operation failed")]
    Store(#[from] ChangesetStoreError),
}

/// Inbound port: enforce that a PR touching governed source carries a covering
/// changeset.
///
/// Implemented by the domain (`ChangesetCheckService`) over the [`Vcs`],
/// [`ChangesetStore`], and the loaded config. Backs the `relman changeset
/// check` `dev`-gate command.
///
/// [`Vcs`]: crate::ports::Vcs
/// [`ChangesetStore`]: crate::ports::ChangesetStore
pub trait ChangesetCheck: Send + Sync {
    /// Check `HEAD` against `base`, returning the report of any violations.
    fn check(&self, base: &str) -> Result<CheckReport, CheckError>;
}
