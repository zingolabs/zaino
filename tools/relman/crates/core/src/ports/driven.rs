use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::types::{CrateName, DateTime, Slug, Utc, Version};

/// Outbound port: the current time.
///
/// The domain depends on this rather than calling `Utc::now()` directly, so
/// services stay deterministic under test (see the `FixedClock` mock). The
/// binary wires a real system-clock adapter.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Everything that can go wrong at the changeset-store I/O boundary.
///
/// Deliberately I/O-only: parsing lives in the domain, so this carries just
/// the low-level failure of touching the `.changesets/` directory.
#[derive(Debug, thiserror::Error)]
pub enum ChangesetStoreError {
    /// An underlying I/O operation on the store failed.
    #[error("changeset store I/O failed for slug {slug:?}")]
    Io {
        /// The slug being operated on, for diagnostics.
        slug: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Listing the store's contents failed.
    #[error("failed to list the changeset store")]
    List {
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Outbound port: the raw store of changeset files.
///
/// Deliberately *dumb* — it moves TOML text in and out of `.changesets/` and
/// nothing else. Parsing/validation is the domain's job, so `read`/`write`
/// traffic in raw `String`s. Slugs are the store's identity: a `Slug` maps to
/// exactly one file (`<slug>.toml`).
pub trait ChangesetStore: Send + Sync {
    /// List the slugs of every changeset currently in the store.
    fn list(&self) -> Result<Vec<Slug>, ChangesetStoreError>;

    /// Report whether a changeset for `slug` already exists.
    fn exists(&self, slug: &Slug) -> Result<bool, ChangesetStoreError>;

    /// Read the raw TOML text of the changeset for `slug`.
    fn read(&self, slug: &Slug) -> Result<String, ChangesetStoreError>;

    /// Write `contents` as the changeset for `slug`, creating the store's
    /// directory if it is missing.
    fn write(&self, slug: &Slug, contents: &str) -> Result<(), ChangesetStoreError>;
}

/// Outbound port: a source of fresh candidate slugs.
///
/// Each call yields a candidate that is *not guaranteed unique* — the domain
/// checks it against the [`ChangesetStore`] and retries on collision. The real
/// adapter draws a random `adjective-noun`; the test mock replays a fixed
/// sequence for determinism.
pub trait SlugSource: Send + Sync {
    /// Produce a fresh candidate slug.
    fn generate(&self) -> Slug;
}

/// Everything that can go wrong querying version control.
///
/// I/O-only, like [`ChangesetStoreError`]: it carries the low-level failure of
/// shelling out to `git`, not any interpretation of the diff (that is the
/// domain's job).
#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    /// The version-control process could not be launched.
    #[error("failed to launch the version-control command")]
    Spawn(#[source] std::io::Error),
    /// The version-control command ran but exited non-zero.
    #[error("version-control command `{command}` failed: {stderr}")]
    Command {
        /// The command line that was run, for diagnostics.
        command: String,
        /// Whatever the command wrote to stderr, trimmed.
        stderr: String,
    },
    /// The command's output was not valid UTF-8.
    #[error("version-control output was not valid UTF-8")]
    Encoding(#[source] std::string::FromUtf8Error),
}

/// Outbound port: the version-control view of what a PR changed.
///
/// The domain depends on this rather than shelling out to `git` directly, so
/// changeset enforcement stays deterministic under test (see the `StubVcs`
/// mock). The binary wires a real git adapter.
pub trait Vcs: Send + Sync {
    /// Repo-relative paths changed on `HEAD` relative to `base` — the PR's
    /// changes. The real adapter computes this as the three-dot diff
    /// (`git diff --name-only <base>...HEAD`), i.e. changes on `HEAD` since its
    /// merge-base with `base`.
    fn changed_files(&self, base: &str) -> Result<Vec<PathBuf>, VcsError>;
}

/// Everything that can go wrong reading the workspace's crate graph.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The backend that reads the workspace (e.g. `cargo metadata`) failed.
    #[error("failed to read the workspace metadata: {message}")]
    Backend {
        /// The rendered backend failure.
        message: String,
    },
    /// A governed target from `relman.toml` was absent from the workspace — the
    /// two manifests have drifted, and silently dropping the target would
    /// under-derive its version.
    #[error("governed target {crate_name:?} was not found in the workspace")]
    MissingTarget {
        /// The target that was declared but not found.
        crate_name: String,
    },
}

/// Outbound port: the workspace's view of governed-crate versions and the
/// dependency edges among them.
///
/// The domain depends on this rather than parsing `Cargo.toml` directly, so
/// version derivation stays deterministic under test (see `MapWorkspace`). The
/// binary wires an adapter backed by `cargo metadata`, which resolves
/// `version.workspace` / `workspace.dependencies` inheritance for us.
///
/// Both methods report **only the governed set** (the targets declared in
/// `relman.toml`): [`versions`](Workspace::versions) yields each target's
/// current version, and [`internal_deps`](Workspace::internal_deps) yields, per
/// target, its dependency edges to *other governed targets* with the declared
/// [`semver::VersionReq`].
pub trait Workspace: Send + Sync {
    /// The current version of each governed target.
    fn versions(&self) -> Result<BTreeMap<CrateName, Version>, WorkspaceError>;

    /// For each governed target `D`, its dependency edges `(T, req)` to other
    /// governed targets `T`, where `req` is the requirement `D` declares on `T`.
    fn internal_deps(
        &self,
    ) -> Result<BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>, WorkspaceError>;
}

/// Everything that can go wrong applying a derived bump to a manifest.
///
/// Parsing is rendered to a `String` (like [`DeriveError::ChangesetParse`]) so
/// the port stays free of any concrete TOML library — the `toml_edit` adapter
/// owns that dependency, the core does not.
///
/// [`DeriveError::ChangesetParse`]: crate::ports::DeriveError::ChangesetParse
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Reading or writing the manifest file failed.
    #[error("manifest I/O failed for {path}")]
    Io {
        /// The manifest path being operated on, for diagnostics.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest text was not well-formed TOML.
    #[error("failed to parse manifest {path} as TOML: {message}")]
    TomlParse {
        /// The manifest path being parsed.
        path: String,
        /// The rendered parse error.
        message: String,
    },
    /// A crate manifest had no `[package]` table, so there is no version to set.
    #[error("manifest {path} has no [package] table")]
    MissingPackageTable {
        /// The offending manifest path.
        path: String,
    },
    /// A crate manifest inherits its version with `version.workspace = true`.
    /// Zaino crates carry a literal per-crate `version`; silently editing an
    /// inherited version would touch the wrong place, so we refuse.
    #[error(
        "manifest {path} sets `version.workspace = true`; \
         relman expects a literal per-crate [package] version"
    )]
    VersionIsWorkspaceInherited {
        /// The offending manifest path.
        path: String,
    },
}

/// Outbound port: format-preserving edits to workspace manifests.
///
/// The domain depends on this rather than parsing `Cargo.toml` itself, so the
/// bump service stays deterministic under test (see `RecordingManifestEditor`).
/// The binary wires a `toml_edit`-backed adapter that mutates exactly one field
/// and preserves all surrounding formatting and comments.
pub trait ManifestEditor: Send + Sync {
    /// Set `[package] version = "<version>"` in the crate manifest at
    /// `manifest_path`. Returns [`ManifestError::VersionIsWorkspaceInherited`]
    /// if the manifest inherits its version rather than carrying a literal one.
    fn set_package_version(
        &self,
        manifest_path: &Path,
        version: &Version,
    ) -> Result<(), ManifestError>;

    /// Update the pinned version of `dep` in the root manifest's
    /// `[workspace.dependencies]`, handling both the string form
    /// (`dep = "0.6.0"`) and the inline-table form
    /// (`dep = { version = "0.6.0", ... }`) — only the version changes, every
    /// other key and all formatting is preserved.
    ///
    /// Returns `Ok(true)` when a version pin was updated and `Ok(false)` when
    /// `dep` carries no version pin to update — either it is absent from
    /// `[workspace.dependencies]` or it is a path-only entry with no `version`
    /// key. Not every governed crate is pinned with a version, so that is a
    /// normal outcome, not an error.
    fn set_workspace_dep_version(
        &self,
        root_manifest: &Path,
        dep: &CrateName,
        version: &Version,
    ) -> Result<bool, ManifestError>;
}
