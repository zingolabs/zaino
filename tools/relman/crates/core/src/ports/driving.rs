use std::path::PathBuf;

use crate::ports::{ChangelogError, ChangesetStoreError, ManifestError, VcsError, WorkspaceError};
use crate::types::{
    AboutReport, BumpTable, CrateName, CycleId, Description, PublishPlan, Slug, TagPlan,
};

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

    /// The slugs of every changeset currently in the store, sorted.
    ///
    /// A read-only listing — it mutates nothing. Backs the dry-run of `relman
    /// changeset clear`, which must show what *would* be removed without
    /// removing it.
    fn list(&self) -> Result<Vec<Slug>, ChangesetsError>;

    /// Rename this PR's author changeset file(s) to the canonical `pr-<pr>`
    /// name(s), returning the new slug(s).
    ///
    /// Backs the `relman changeset rename --pr <N>` step the PR-gate bot runs.
    /// "This PR's files" are the changesets whose slug is *not* already a
    /// [canonical PR name](Slug::is_canonical_pr) — the author's random slug(s);
    /// accumulated `pr-*` files from earlier merged PRs are left untouched. The
    /// non-canonical sources are renamed in sorted order: the first becomes
    /// `pr-<pr>`, the second `pr-<pr>-2`, and so on
    /// ([`Slug::for_pr`](crate::types::Slug::for_pr)). A pre-existing target
    /// name is an error. Zero author files is a no-op returning an empty vec —
    /// the bot may safely re-run, since renaming is idempotent once canonical.
    fn rename_to_pr(&self, pr: u32) -> Result<Vec<Slug>, ChangesetsError>;

    /// Remove *every* changeset file, returning the removed slugs (sorted).
    ///
    /// The release "consume" step: after a release PR merges, `relman` clears
    /// `.changesets/` so the next cycle starts empty. This is the one
    /// irreversible lifecycle step and must run only at a true release.
    fn clear(&self) -> Result<Vec<Slug>, ChangesetsError>;
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
    /// A this-PR changeset file is an unfilled template — a `changeset new`
    /// scaffold that declares neither `[[changes]]` nor `[empty]`, so it covers
    /// nothing. Distinct from [`ChangesetParse`](Violation::ChangesetParse): the
    /// file is not broken, just not yet filled in.
    UnfilledTemplate(PathBuf),
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
            Violation::UnfilledTemplate(file) => {
                format!(
                    "unfilled changeset template `{}`: fill in a [[changes]] block or run \
                     `relman changeset new --empty \"<reason>\"`",
                    file.display()
                )
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

/// Everything that can go wrong deriving the per-crate version bumps.
///
/// Unlike changeset *enforcement* (which returns non-compliance as data), a
/// derivation failure means we could not produce a correct table at all — a
/// broken changeset, an unknown target, or a workspace/store I/O failure — so
/// each is a hard error rather than a `Violation`.
#[derive(Debug, thiserror::Error)]
pub enum DeriveError {
    /// A changeset in the store could not be parsed.
    #[error("failed to parse changeset {slug:?}: {error}")]
    ChangesetParse {
        /// The slug of the offending changeset.
        slug: String,
        /// The rendered parse error.
        error: String,
    },
    /// A changeset entry named a `crate` that is not a declared target. We fail
    /// rather than silently drop it, so a mis-targeted bump can never vanish.
    #[error("changeset entry names {crate_name:?}, which is not a declared target")]
    UnknownTarget {
        /// The undeclared crate name.
        crate_name: String,
    },
    /// A crate that must bump has no known current version in the workspace.
    #[error("no workspace version found for {crate_name:?}")]
    MissingVersion {
        /// The crate whose version could not be resolved.
        crate_name: String,
    },
    /// Reading the workspace metadata failed.
    #[error("workspace query failed")]
    Workspace(#[from] WorkspaceError),
    /// A changeset-store operation failed.
    #[error("changeset store operation failed")]
    Store(#[from] ChangesetStoreError),
}

/// Inbound port: derive the per-crate version bump table from the accumulated
/// changesets and the workspace crate graph.
///
/// Implemented by the domain (`VersionService`) over the [`ChangesetStore`] and
/// [`Workspace`] driven ports and the loaded config. Read-only over the *whole*
/// `.changesets/` set — it never consumes or clears.
///
/// [`ChangesetStore`]: crate::ports::ChangesetStore
/// [`Workspace`]: crate::ports::Workspace
pub trait Versions: Send + Sync {
    /// Aggregate changesets into direct + transitive per-crate bumps.
    ///
    /// Tolerates unfilled templates (a `changeset new` scaffold not yet edited,
    /// which parses to an empty document): they contribute nothing and are
    /// skipped, never failing the derivation. A *malformed* changeset is still a
    /// hard [`DeriveError::ChangesetParse`].
    fn derive(&self) -> Result<BumpTable, DeriveError>;

    /// The repo-relative paths of unfilled changeset templates in the store —
    /// files that parse to an empty document (a `changeset new` scaffold not yet
    /// edited). [`derive`](Versions::derive) skips these silently; this reports
    /// them so a delivery mechanism can warn the user that a template was left
    /// unfilled. Malformed changesets are excluded — they are surfaced as hard
    /// errors by `derive`, not as skippable templates here.
    fn unfilled_templates(&self) -> Result<Vec<PathBuf>, DeriveError>;
}

/// Everything that can go wrong applying a derived [`BumpTable`] to the
/// workspace manifests.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A `CrateBump` named a crate that is not a declared target, so no manifest
    /// path could be resolved for it. We fail rather than silently skip, so a
    /// mis-targeted bump can never vanish.
    #[error("bump names {crate_name:?}, which is not a declared target")]
    UnknownTarget {
        /// The undeclared crate name.
        crate_name: String,
    },
    /// Editing one of the manifests failed.
    #[error("manifest edit failed")]
    Manifest(#[from] ManifestError),
}

/// Inbound port: mechanically apply a derived [`BumpTable`] to the workspace
/// manifests.
///
/// Implemented by the domain (`BumpService`) over the [`ManifestEditor`] driven
/// port and the loaded config: it sets each bumped crate's `[package] version`
/// and updates the matching root `[workspace.dependencies]` pin. Named
/// `ApplyBump` (not `Bump`) to avoid clashing with the [`Bump`] *type*.
///
/// [`ManifestEditor`]: crate::ports::ManifestEditor
/// [`Bump`]: crate::types::Bump
pub trait ApplyBump: Send + Sync {
    /// Apply every [`CrateBump`] in `table` to the manifests.
    ///
    /// [`CrateBump`]: crate::types::CrateBump
    fn apply(&self, table: &BumpTable) -> Result<(), ApplyError>;
}

/// A single planned changelog edit: the target file, its complete new contents,
/// and — for display — just the section that was spliced in.
///
/// [`Changelog::generate`] returns these without touching disk;
/// [`Changelog::apply`] returns the same set after writing them. `inserted`
/// lets a `--dry-run` show exactly what would be added without diffing whole
/// files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEdit {
    path: PathBuf,
    contents: String,
    inserted: String,
}

impl ChangelogEdit {
    /// Construct from the target path, the full new file contents, and the
    /// rendered section that was inserted.
    pub fn new(path: PathBuf, contents: String, inserted: String) -> Self {
        Self {
            path,
            contents,
            inserted,
        }
    }

    /// The changelog file this edit targets (repo-relative).
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// The complete new contents to write.
    pub fn contents(&self) -> &str {
        &self.contents
    }

    /// Just the newly-inserted section, for display.
    pub fn inserted(&self) -> &str {
        &self.inserted
    }
}

/// Everything that can go wrong generating changelog edits.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogGenError {
    /// Deriving the per-crate bump table failed.
    #[error("version derivation failed")]
    Derive(#[from] DeriveError),
    /// A changeset in the store could not be parsed while gathering its entries.
    #[error("failed to parse changeset {slug:?}: {error}")]
    ChangesetParse {
        /// The slug of the offending changeset.
        slug: String,
        /// The rendered parse error.
        error: String,
    },
    /// A changeset-store operation failed.
    #[error("changeset store operation failed")]
    ChangesetStore(#[from] ChangesetStoreError),
    /// A changelog-store operation failed.
    #[error("changelog store operation failed")]
    Changelog(#[from] ChangelogError),
}

/// Inbound port: generate Keep-a-Changelog entries for each bumping crate and
/// the workspace, from the accumulated changesets.
///
/// Implemented by the domain (`ChangelogService`) over the [`Versions`] and
/// [`ChangesetStore`] and [`ChangelogStore`] driven ports, a [`Clock`], and the
/// loaded config. [`generate`](Changelog::generate) plans the edits without
/// touching disk; [`apply`](Changelog::apply) writes them and returns the same
/// set.
///
/// [`ChangesetStore`]: crate::ports::ChangesetStore
/// [`ChangelogStore`]: crate::ports::ChangelogStore
/// [`Clock`]: crate::ports::Clock
pub trait Changelog: Send + Sync {
    /// Plan the changelog edits (per bumping crate + the workspace) without
    /// writing anything.
    fn generate(&self) -> Result<Vec<ChangelogEdit>, ChangelogGenError>;

    /// Generate the edits and write them through the [`ChangelogStore`],
    /// returning what was written.
    ///
    /// [`ChangelogStore`]: crate::ports::ChangelogStore
    fn apply(&self) -> Result<Vec<ChangelogEdit>, ChangelogGenError>;
}

/// Everything that can go wrong computing a release artifact (tag plan, PR body,
/// or publish plan).
///
/// These commands are pure planners — they read the derived [`BumpTable`], the
/// changesets, and the crate graph, and print a plan for CI to apply. A failure
/// means the plan could not be *computed* correctly, so each variant is a hard
/// error rather than data.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// Deriving the per-crate bump table failed.
    #[error("version derivation failed")]
    Derive(#[from] DeriveError),
    /// A changeset in the store could not be parsed while gathering its entries
    /// for the changelog block.
    #[error("failed to parse changeset {slug:?}: {error}")]
    ChangesetParse {
        /// The slug of the offending changeset.
        slug: String,
        /// The rendered parse error.
        error: String,
    },
    /// A changeset-store operation failed.
    #[error("changeset store operation failed")]
    Store(#[from] ChangesetStoreError),
    /// Reading the workspace crate graph failed.
    #[error("workspace query failed")]
    Workspace(#[from] WorkspaceError),
    /// The governed dependency graph among the bumping crates contains a cycle,
    /// so no publish order exists. This should never happen for a real Cargo
    /// workspace (Cargo itself forbids dependency cycles), but detecting it
    /// keeps the topological sort from looping forever.
    #[error("dependency cycle among bumping crates: no publish order exists")]
    DependencyCycle,
}

/// Inbound port: compute the release artifacts CI applies at a soak cut or a
/// blessing — the git tag plan, the release-PR body, and the publish order.
///
/// Implemented by the domain (`ReleaseArtifactsService`) over the [`Versions`]
/// driving port, the [`ChangesetStore`] and [`Workspace`] driven ports, and the
/// loaded config. Every method is a pure planner: it computes and returns, and
/// never mutates a ref, the working tree, or a registry.
///
/// [`ChangesetStore`]: crate::ports::ChangesetStore
/// [`Workspace`]: crate::ports::Workspace
pub trait ReleaseArtifacts: Send + Sync {
    /// The tag plan for `cycle`.
    ///
    /// - `rc = Some(n)` (a soak/prerelease cut): a single `cycle-<id>-rc.<n>`
    ///   prerelease tag.
    /// - `rc = None` (a blessing): the `cycle-<id>` period tag followed by one
    ///   `<crate>-v<next>` provenance tag per bumping crate, in config order.
    fn tags(&self, cycle: &CycleId, rc: Option<u32>) -> Result<TagPlan, ArtifactError>;

    /// The rendered release-PR body for `cycle`: a title, the derived version
    /// table, a CI-filled soak-status placeholder, and the aggregated changelog.
    fn pr_body(&self, cycle: &CycleId) -> Result<String, ArtifactError>;

    /// The bumping crates in dependency (publish) order.
    fn publish_plan(&self) -> Result<PublishPlan, ArtifactError>;
}
