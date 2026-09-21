//! Failures the runtime can report.

/// The chain head could not advance against its source.
///
/// The non-finalised state's `SyncError` and `UpdateError`, merged and renamed:
/// there is no sync operation here to name, and the two enums split along a
/// boundary — "extending failed" versus "publishing failed" — that no longer
/// exists now that building and publishing are separate steps.
///
/// Gone with their collaborators: `CannotReadFinalizedState` (no finalised
/// state), `StagingChannelClosed` (no staging channel), and `StaleSnapshot`
/// (one writer, so nothing to lose a race against).
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadAdvanceError {
    /// The validator could not be reached, or failed the request.
    ///
    /// Transient by assumption: the writer task backs off and retries, and only
    /// escalates after a run of them.
    #[error("validator unavailable: {0}")]
    SourceUnavailable(String),

    /// The validator answered, but with data that cannot be reconciled — a
    /// block missing whose child it just served, a header whose difficulty does
    /// not decode.
    ///
    /// Retrying may help if the validator was mid-reorg; persisting means one
    /// side is wrong.
    #[error("validator returned inconsistent data: {0}")]
    InconsistentSource(String),

    /// A reorg could not be resolved within the retained window.
    #[error("reorg failed: {0}")]
    ReorgFailure(String),
}

/// The chain head could not anchor, so it has no graph at all.
///
/// Construction is fallible because a chain head is nothing without a window:
/// it holds no persistent state to fall back on and has no other data source,
/// so one that cannot reach its validator has nothing to offer. Failing here
/// rather than existing in a degraded state is what lets `current()` be total
/// for the rest of the runtime's life.
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadInitError {
    /// The validator could not be reached for the configured number of
    /// consecutive attempts.
    #[error("chain head could not anchor after {attempts} attempts: {source}")]
    SourceUnavailable {
        /// How many attempts were made before giving up.
        attempts: u32,
        /// The last failure seen.
        source: ChainHeadAdvanceError,
    },

    /// Anchoring was cancelled before it completed.
    #[error("chain head anchoring cancelled")]
    Cancelled,
}
