use crate::types::{DateTime, Slug, Utc};

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
