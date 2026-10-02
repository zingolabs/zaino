//! Per-capability error types.
//!
//! Distinct types (not one god-enum) so each outer client maps precisely, and
//! so capabilities can diverge later — the CLAUDE.md "per-conversion error
//! granularity" rule, applied to the driving surface. In this scaffold they
//! share a shape; a macro keeps them DRY (a `fn` cannot define types).

use crate::Capability;
use zaino_primitives::types::TransparentInput;

/// Every read-boundary failure separates a *not-yet-serviceable* answer and a
/// *domain* "not found" (which is `Ok(None)`, never an error) from real backend
/// failure classified transient/fatal (the PR's *Transient failure* glossary).
macro_rules! read_error {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, thiserror::Error)]
        pub enum $name {
            /// Backing index not built to the requested height yet.
            #[error("not serviceable yet: {0:?}")]
            NotServiceable(Capability),
            /// Likely to resolve on retry (e.g. a mid-swap reorg race).
            #[error("transient read failure: {0}")]
            Transient(String),
            /// Unrecoverable backend failure.
            #[error("fatal read failure: {0}")]
            Fatal(String),
        }

        impl $name {
            /// True only for the not-yet-serviceable stub — the read's backing
            /// substrate is unwired, distinct from a real backend failure
            /// (`Transient` / `Fatal`) or a domain miss (`Ok(None)`). The
            /// conformance kit asserts this is false across a use case's read-set:
            /// a served capability may answer, miss, or fail, but never report
            /// itself unserviceable.
            pub fn is_not_serviceable(&self) -> bool {
                matches!(self, Self::NotServiceable(_))
            }
        }
    };
}

read_error!(/// Errors from [`crate::BlockRead`].
    BlockReadError);
read_error!(/// Errors from [`crate::TransactionRead`].
    TxReadError);
read_error!(/// Errors from [`crate::TreestateRead`].
    TreestateReadError);
read_error!(/// Errors from [`crate::AddressRead`].
    AddressReadError);
read_error!(/// Errors from [`crate::SpendRead`].
    SpendReadError);
read_error!(/// Errors from [`crate::MempoolContent`].
    MempoolReadError);
read_error!(/// Generic read error for streamed surfaces and [`crate::ForkReconcile`].
    ReadError);

/// A [`BlockReadError`] folds into the generic [`ReadError`] 1:1 — same three
/// cases. A per-height block read ([`crate::BlockRead::block`]) fails with
/// [`BlockReadError`], while the streamed read ([`crate::BlockRead::stream_blocks`])
/// yields [`ReadError`], so a per-height failure lifts into the stream's error
/// with no reclassification. Context-free, so `From` is the right tool: the
/// named-method boundary rule governs `Persistent*`/`proto::` conversions, not
/// two business error types.
impl From<BlockReadError> for ReadError {
    fn from(error: BlockReadError) -> Self {
        match error {
            BlockReadError::NotServiceable(capability) => Self::NotServiceable(capability),
            BlockReadError::Transient(message) => Self::Transient(message),
            BlockReadError::Fatal(message) => Self::Fatal(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockReadError, ReadError};
    use crate::Capability;

    #[test]
    fn block_read_error_lifts_into_read_error_by_variant() {
        assert!(matches!(
            ReadError::from(BlockReadError::NotServiceable(Capability::Blocks)),
            ReadError::NotServiceable(Capability::Blocks)
        ));
        assert!(matches!(
            ReadError::from(BlockReadError::Transient("race".to_owned())),
            ReadError::Transient(message) if message == "race"
        ));
        assert!(matches!(
            ReadError::from(BlockReadError::Fatal("corrupt".to_owned())),
            ReadError::Fatal(message) if message == "corrupt"
        ));
    }
}

/// Failure to *acquire* a snapshot — the one reorg race ADR-0003 admits.
/// Reads *through* a snapshot never race, so they never yield this.
#[derive(Debug, thiserror::Error)]
#[error("could not acquire a snapshot: {0}")]
pub struct Transient(pub String);

/// A broadcast rejection is a *domain answer*, not a backend failure.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastRejection {
    /// Bytes did not decode to a transaction.
    #[error("malformed transaction: {0}")]
    Malformed(String),
    /// Decoded, but consensus/validation rejected it (with the engine's reason).
    #[error("transaction rejected: {0}")]
    Invalid(String),
}

/// Errors from [`crate::TransactionViewRead`].
///
/// A typed enum, not a `read_error!` String type: resolving a transparent input
/// to the output it spends has failure modes the shared three-case shape cannot
/// name. A miss on the *requested* transaction or block is a domain answer
/// (`Ok(None)`, never a variant here); the variants are the inconsistencies and
/// transport failures that can arise while resolving the inputs of a transaction
/// the validator *did* serve. The transport variant keeps its cause with
/// `#[source]`, so an operator walking the chain reaches the concrete failure.
#[derive(Debug, thiserror::Error)]
pub enum TransactionViewError {
    /// The validator could not be reached or answered unusably.
    #[error("validator unavailable")]
    Unavailable {
        /// The transport failure, kept as the source chain.
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The validator served a spending transaction but not the transaction it
    /// spends — a source inconsistency, named rather than rendered as a blank
    /// input value.
    #[error("prevout {outpoint:?} is unknown to the validator")]
    MissingPrevout {
        /// The input whose referenced output could not be found.
        outpoint: TransparentInput,
    },
    /// Resolving this request would fetch more distinct prevout transactions than
    /// the passthrough-phase ceiling allows — a policy refusal, not a transport
    /// failure: the request is well-formed and the validator healthy, but serving
    /// it by per-prevout passthrough would fan out beyond the bound. Distinct from
    /// [`Unavailable`](Self::Unavailable), which is a validator that could not be
    /// reached. A local outpoint index removes the fetch and lifts this ceiling.
    #[error(
        "resolving this request needs {needed} prevout fetches, over the per-request ceiling of {ceiling}"
    )]
    PrevoutFanoutTooLarge {
        /// The number of distinct external prevout transactions the request would
        /// fetch.
        needed: usize,
        /// The per-request ceiling that was exceeded.
        ceiling: usize,
    },
    /// The spent transaction exists but has no output at the referenced index.
    #[error("prevout {outpoint:?} names output {index} of a transaction with {outputs} outputs")]
    PrevoutIndexOutOfRange {
        /// The input whose referenced output index is out of range.
        outpoint: TransparentInput,
        /// The referenced output index.
        index: u32,
        /// The number of outputs the spent transaction actually has.
        outputs: usize,
    },
}
