//! Query: fetch the current best chain tip.

use core::fmt;
use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use super::TransportError;

/// Domain error for [`GetChainTip`].
///
/// There are no domain-level failure modes for "what is the tip?" beyond
/// transport failure — the validator always has a tip. This enum exists
/// for forward compatibility (e.g. a validator that reports "still syncing").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetChainTipError {
    /// The validator is not ready to report a tip (e.g. still syncing).
    NotReady,
}

impl fmt::Display for GetChainTipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady => write!(f, "validator not ready"),
        }
    }
}

/// Fetch the current best chain tip (hash + height).
///
/// Maps to `getbestblockhash()` + `getblock(hash, 0)` over JSON-RPC,
/// or the equivalent ReadState query.
pub trait GetChainTip: Send + Sync {
    /// Fetch current tip.
    fn get_chain_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError>> + Send;
}

/// Combined domain + transport error for this query.
#[derive(Debug)]
pub enum QueryError {
    /// Domain-level failure.
    Domain(GetChainTipError),
    /// Transport-level failure.
    Transport(TransportError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(e) => write!(f, "{e}"),
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<GetChainTipError> for QueryError {
    fn from(e: GetChainTipError) -> Self {
        Self::Domain(e)
    }
}

impl From<TransportError> for QueryError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}
