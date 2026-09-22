//! Per-capability error types.
//!
//! Distinct types (not one god-enum) so each outer client maps precisely, and
//! so capabilities can diverge later — the CLAUDE.md "per-conversion error
//! granularity" rule, applied to the driving surface. In this scaffold they
//! share a shape; a macro keeps them DRY (a `fn` cannot define types).

use zaino_core::Capability;

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
            /// conformance kit asserts this is false across a profile's read-set:
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
read_error!(/// Generic read error for streamed surfaces and [`crate::ForkReconcile`].
    ReadError);

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
