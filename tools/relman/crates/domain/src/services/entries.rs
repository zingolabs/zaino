use std::collections::BTreeMap;

use relman_core::ports::{ArtifactError, ChangelogGenError, ChangesetStore, ChangesetStoreError};
use relman_core::types::{
    ChangeEntry, Changeset, ChangesetError, ConsumedLedger, CrateName, StoredChangeset,
};

/// Why the changeset entries could not be gathered.
#[derive(Debug, thiserror::Error)]
pub(super) enum EntriesError {
    /// A changeset in the store could not be parsed.
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
}

impl From<EntriesError> for ChangelogGenError {
    fn from(error: EntriesError) -> Self {
        match error {
            EntriesError::ChangesetParse { slug, error } => Self::ChangesetParse { slug, error },
            EntriesError::Store(error) => Self::ChangesetStore(error),
        }
    }
}

impl From<EntriesError> for ArtifactError {
    fn from(error: EntriesError) -> Self {
        match error {
            EntriesError::ChangesetParse { slug, error } => Self::ChangesetParse { slug, error },
            EntriesError::Store(error) => Self::Store(error),
        }
    }
}

/// Groups each crate's unshipped changeset entries in sorted-slug order.
pub(super) fn entries_by_crate<S: ChangesetStore + ?Sized>(
    changesets: &S,
    ledger: &ConsumedLedger,
) -> Result<BTreeMap<CrateName, Vec<ChangeEntry>>, EntriesError> {
    let mut slugs = changesets.list()?;
    slugs.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    let mut by_crate: BTreeMap<CrateName, Vec<ChangeEntry>> = BTreeMap::new();
    for slug in &slugs {
        let raw = changesets.read(slug)?;
        let stored = match StoredChangeset::parse_toml(&raw) {
            Ok(stored) => stored,
            // An unfilled template carries no entries, exactly like an `Empty`
            // changeset below; the version derivation surfaces the warning.
            Err(ChangesetError::Unfilled) => continue,
            Err(error) => {
                return Err(EntriesError::ChangesetParse {
                    slug: slug.as_str().to_owned(),
                    error: error.to_string(),
                });
            }
        };
        // A shipped changeset belongs to a past release, whether marked in-file
        // or only known shipped through the ledger; it never re-lists.
        if stored.consumed_in().is_some() || stored.id().is_some_and(|id| ledger.contains(id)) {
            continue;
        }
        let Changeset::WithChanges(entries) = stored.into_body() else {
            continue;
        };
        for entry in entries {
            by_crate
                .entry(entry.crate_name().clone())
                .or_default()
                .push(entry);
        }
    }
    Ok(by_crate)
}
