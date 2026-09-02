//! Driving the store through its ports.
//!
//! The two calls that make the store *do* something rather than answer: build
//! itself up to a height, and close. Kept apart from the reads because they
//! belong to a different caller — the sync worker owns these, where every RPC
//! path owns the reads — and because they are the two that can fail as the
//! *source's* fault rather than the store's.

use zaino_chain_store::{ChainStoreError, ChainStoreIngest, ChainStoreSourceError};

use super::reading::domain_height;

/// Builds the store up to `target`.
///
/// Generic over [`ChainStoreIngest`] for the same reason the reads are generic
/// over their ports: it is what stops the sync worker reaching for an inherent
/// method that a second store would not have.
pub(crate) async fn build_to<I: ChainStoreIngest>(
    ingest: &I,
    target: crate::Height,
) -> Result<(), ChainStoreSourceError> {
    // A target beyond the protocol maximum cannot name a block, so there is
    // nothing to build to. The sync worker derives this from the validator's
    // tip, so reaching it would mean the validator reported an impossible
    // height — which is the source's inconsistency, not the store's.
    let Some(target) = domain_height(target) else {
        return Err(ChainStoreSourceError::inconsistent_data(format!(
            "validator reported height {target}, which is above the protocol maximum"
        )));
    };
    ingest.build_to(target).await
}

/// Closes the store.
pub(crate) async fn shutdown<I: ChainStoreIngest>(ingest: &I) -> Result<(), ChainStoreError> {
    ingest.shutdown().await
}
