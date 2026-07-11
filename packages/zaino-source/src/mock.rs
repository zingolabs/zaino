//! In-memory mock adapter for testing.
//!
//! Implements the query traits against a pre-populated chain.
//! Supports failure injection: configure the mock to fail N times
//! with a specific [`FailureMode`] before succeeding, exercising
//! the resilience wrapper.
//!
//! ```ignore
//! let mock = MockChain::new()
//!     .with_block(height, hash, bytes)
//!     .fail_next(2, FailureMode::Timeout); // first 2 calls fail
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zaino_primitives::types::{BlockHash, Height, Treestate};

use crate::error::{FailureMode, FetchError};
use crate::{
    GetBlockByHashError, GetBlockBytesError, GetChainTipError, GetTreestateError, QueryError,
};

/// A pre-populated in-memory chain for testing.
pub struct MockChain {
    blocks: HashMap<u32, MockBlock>,
    by_hash: HashMap<[u8; 32], u32>,
    tip: Option<(BlockHash, Height)>,
    treestates: HashMap<u32, Treestate>,
    /// Number of remaining failures to inject before succeeding.
    failures_remaining: AtomicU32,
    /// What kind of failure to inject.
    failure_mode: FailureMode,
}

struct MockBlock {
    bytes: Vec<u8>,
}

impl MockChain {
    /// Empty chain, no failure injection.
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            by_hash: HashMap::new(),
            tip: None,
            treestates: HashMap::new(),
            failures_remaining: AtomicU32::new(0),
            failure_mode: FailureMode::Connection,
        }
    }

    /// Add a block. The last block added becomes the tip.
    pub fn with_block(mut self, height: Height, hash: BlockHash, bytes: Vec<u8>) -> Self {
        let h = u32::from(height);
        self.by_hash.insert(<[u8; 32]>::from(hash), h);
        self.blocks.insert(h, MockBlock { bytes });
        self.tip = Some((hash, height));
        self
    }

    /// Add a treestate at a height.
    pub fn with_treestate(mut self, height: Height, treestate: Treestate) -> Self {
        self.treestates.insert(u32::from(height), treestate);
        self
    }

    /// Inject `count` failures with the given mode before the next
    /// successful call. Each query trait method decrements the counter
    /// and returns a [`FetchError`] until it reaches zero.
    pub fn fail_next(self, count: u32, mode: FailureMode) -> Self {
        self.failures_remaining.store(count, Ordering::SeqCst);
        Self {
            failure_mode: mode,
            ..self
        }
    }

    /// If failures remain, decrement and return an error.
    fn maybe_fail<E: core::fmt::Debug + core::fmt::Display>(
        &self,
    ) -> Option<QueryError<E>> {
        let prev = self.failures_remaining.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |n| if n > 0 { Some(n - 1) } else { None },
        );
        match prev {
            Ok(_) => Some(QueryError::Fetch(FetchError::new(
                self.failure_mode.clone(),
                format!("mock injected {:?}", self.failure_mode),
            ))),
            Err(_) => None,
        }
    }
}

impl Default for MockChain {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::GetBlockBytes for MockChain {
    async fn get_block_bytes(
        &self,
        height: Height,
    ) -> Result<Vec<u8>, QueryError<GetBlockBytesError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.blocks
            .get(&u32::from(height))
            .map(|b| b.bytes.clone())
            .ok_or(QueryError::Domain(GetBlockBytesError::HeightNotFound(
                height,
            )))
    }
}

impl crate::GetBlockByHash for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        let h = self
            .by_hash
            .get(&<[u8; 32]>::from(hash))
            .and_then(|h| self.blocks.get(h));
        match h {
            Some(b) => Ok(b.bytes.clone()),
            None => Err(QueryError::Domain(GetBlockByHashError::NotFound(hash))),
        }
    }
}

impl crate::GetChainTip for MockChain {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.tip
            .ok_or(QueryError::Domain(GetChainTipError::NotReady))
    }
}

impl crate::GetTreestate for MockChain {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.treestates
            .get(&u32::from(height))
            .cloned()
            .ok_or(QueryError::Domain(GetTreestateError::HeightNotFound(
                height,
            )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn hash(byte: u8) -> BlockHash {
        BlockHash::from([byte; 32])
    }

    #[tokio::test]
    async fn tip_of_empty_chain_is_not_ready() {
        let mock = MockChain::new();
        let err = crate::GetChainTip::get_chain_tip(&mock).await.unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetChainTipError::NotReady)
        ));
    }

    #[tokio::test]
    async fn get_block_bytes_roundtrip() {
        let mock = MockChain::new().with_block(height(0), hash(1), vec![0xDE, 0xAD]);
        let bytes = crate::GetBlockBytes::get_block_bytes(&mock, height(0))
            .await
            .expect("block exists");
        assert_eq!(bytes, vec![0xDE, 0xAD]);
    }

    #[tokio::test]
    async fn get_block_bytes_not_found() {
        let mock = MockChain::new();
        let err = crate::GetBlockBytes::get_block_bytes(&mock, height(99))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockBytesError::HeightNotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_block_by_hash_roundtrip() {
        let mock = MockChain::new().with_block(height(0), hash(1), vec![0xBE, 0xEF]);
        let bytes = crate::GetBlockByHash::get_block_by_hash(&mock, hash(1))
            .await
            .expect("block exists");
        assert_eq!(bytes, vec![0xBE, 0xEF]);
    }

    #[tokio::test]
    async fn tip_is_last_added_block() {
        let mock = MockChain::new()
            .with_block(height(0), hash(1), vec![])
            .with_block(height(1), hash(2), vec![]);
        let (tip_hash, tip_height) = crate::GetChainTip::get_chain_tip(&mock)
            .await
            .expect("has tip");
        assert_eq!(tip_hash, hash(2));
        assert_eq!(tip_height, height(1));
    }

    #[tokio::test]
    async fn treestate_roundtrip() {
        let ts = Treestate {
            sapling: Some(vec![1, 2, 3]),
            orchard: None,
        };
        let mock = MockChain::new()
            .with_block(height(0), hash(1), vec![])
            .with_treestate(height(0), ts.clone());
        let result = crate::GetTreestate::get_treestate(&mock, height(0))
            .await
            .expect("treestate exists");
        assert_eq!(result.sapling, Some(vec![1, 2, 3]));
        assert!(result.orchard.is_none());
    }

    #[tokio::test]
    async fn injected_failure_then_success() {
        let mock = MockChain::new()
            .with_block(height(0), hash(1), vec![0xAB])
            .fail_next(1, FailureMode::Timeout);

        // First call fails.
        let err = crate::GetBlockBytes::get_block_bytes(&mock, height(0))
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::Fetch(ref e) if e.mode == FailureMode::Timeout));

        // Second call succeeds.
        let bytes = crate::GetBlockBytes::get_block_bytes(&mock, height(0))
            .await
            .expect("succeeds after failure consumed");
        assert_eq!(bytes, vec![0xAB]);
    }

    #[tokio::test]
    async fn multiple_injected_failures() {
        let mock = MockChain::new()
            .with_block(height(0), hash(1), vec![0xCD])
            .fail_next(3, FailureMode::Connection);

        for _ in 0..3 {
            let err = crate::GetBlockBytes::get_block_bytes(&mock, height(0))
                .await
                .unwrap_err();
            assert!(matches!(err, QueryError::Fetch(_)));
        }

        // Fourth call succeeds.
        let bytes = crate::GetBlockBytes::get_block_bytes(&mock, height(0))
            .await
            .expect("succeeds after 3 failures");
        assert_eq!(bytes, vec![0xCD]);
    }
}
