//! In-memory mock adapter for testing.
//!
//! Implements the query traits against a pre-populated chain.
//! Supports failure injection for resilience testing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zaino_primitives::types::{Block, BlockHash, Height, Treestate};

use crate::error::{FailureMode, FetchError};
use crate::{GetBlockByHashError, GetBlockError, GetChainTipError, GetTreestateError, QueryError};

/// A pre-populated in-memory chain for testing.
pub struct MockChain {
    blocks: HashMap<u32, Block>,
    by_hash: HashMap<[u8; 32], u32>,
    tip: Option<(BlockHash, Height)>,
    treestates: HashMap<u32, Treestate>,
    failures_remaining: AtomicU32,
    failure_mode: FailureMode,
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
    pub fn with_block(mut self, block: Block) -> Self {
        let height = u32::from(block.header.height);
        let hash = block.header.hash;
        self.by_hash.insert(<[u8; 32]>::from(hash), height);
        self.tip = Some((hash, block.header.height));
        self.blocks.insert(height, block);
        self
    }

    /// Add a treestate at a height.
    pub fn with_treestate(mut self, height: Height, treestate: Treestate) -> Self {
        self.treestates.insert(u32::from(height), treestate);
        self
    }

    /// Inject `count` failures with the given mode before the next
    /// successful call.
    pub fn fail_next(self, count: u32, mode: FailureMode) -> Self {
        self.failures_remaining.store(count, Ordering::SeqCst);
        Self {
            failure_mode: mode,
            ..self
        }
    }

    fn maybe_fail<E: core::fmt::Debug + core::fmt::Display>(&self) -> Option<QueryError<E>> {
        let prev = self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n > 0 {
                    Some(n - 1)
                } else {
                    None
                }
            });
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

impl crate::GetBlock for MockChain {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.blocks
            .get(&u32::from(height))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl crate::GetBlockByHash for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        let block = self
            .by_hash
            .get(&<[u8; 32]>::from(hash))
            .and_then(|h| self.blocks.get(h));
        match block {
            Some(b) => Ok(b.clone()),
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
    use zaino_primitives::types::{BlockHeader, ChainMetadata};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn hash(byte: u8) -> BlockHash {
        BlockHash::from([byte; 32])
    }

    /// Build a minimal test block at a given height with a given hash.
    fn test_block(h: u32, hash_byte: u8) -> Block {
        Block {
            header: BlockHeader {
                hash: hash(hash_byte),
                prev_hash: BlockHash::ZERO,
                height: height(h),
                time: 0,
                merkle_root: [0; 32].into(),
                block_commitments: [0; 32].into(),
                bits: 0,
                nonce: [0; 32],
            },
            transactions: vec![],
            chain_metadata: ChainMetadata {
                sapling_tree_size: 0,
                orchard_tree_size: 0,
                ironwood_tree_size: 0,
            },
        }
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
    async fn get_block_roundtrip() {
        let mock = MockChain::new().with_block(test_block(0, 1));
        let block = crate::GetBlock::get_block(&mock, height(0))
            .await
            .expect("block exists");
        assert_eq!(block.header.hash, hash(1));
    }

    #[tokio::test]
    async fn get_block_not_found() {
        let mock = MockChain::new();
        let err = crate::GetBlock::get_block(&mock, height(99))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockError::HeightNotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_block_by_hash_roundtrip() {
        let mock = MockChain::new().with_block(test_block(0, 1));
        let block = crate::GetBlockByHash::get_block_by_hash(&mock, hash(1))
            .await
            .expect("block exists");
        assert_eq!(block.header.height, height(0));
    }

    #[tokio::test]
    async fn tip_is_last_added_block() {
        let mock = MockChain::new()
            .with_block(test_block(0, 1))
            .with_block(test_block(1, 2));
        let (tip_hash, tip_height) = crate::GetChainTip::get_chain_tip(&mock)
            .await
            .expect("has tip");
        assert_eq!(tip_hash, hash(2));
        assert_eq!(tip_height, height(1));
    }

    #[tokio::test]
    async fn treestate_roundtrip() {
        let ts = Treestate {
            block_hash: hash(1),
            height: height(0),
            time: 0,
            sapling: Some(zaino_primitives::types::PoolTreestate {
                final_root: None,
                final_state: vec![1, 2, 3],
            }),
            orchard: None,
            ironwood: None,
        };
        let mock = MockChain::new()
            .with_block(test_block(0, 1))
            .with_treestate(height(0), ts);
        let result = crate::GetTreestate::get_treestate(&mock, height(0))
            .await
            .expect("treestate exists");
        assert_eq!(
            result.sapling.map(|pool| pool.final_state),
            Some(vec![1, 2, 3])
        );
        assert!(result.orchard.is_none());
    }

    #[tokio::test]
    async fn injected_failure_then_success() {
        let mock = MockChain::new()
            .with_block(test_block(0, 1))
            .fail_next(1, FailureMode::Timeout);

        let err = crate::GetBlock::get_block(&mock, height(0))
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::Fetch(ref e) if e.mode == FailureMode::Timeout));

        let block = crate::GetBlock::get_block(&mock, height(0))
            .await
            .expect("succeeds after failure consumed");
        assert_eq!(block.header.hash, hash(1));
    }

    #[tokio::test]
    async fn multiple_injected_failures() {
        let mock = MockChain::new()
            .with_block(test_block(0, 1))
            .fail_next(3, FailureMode::Connection);

        for _ in 0..3 {
            let err = crate::GetBlock::get_block(&mock, height(0))
                .await
                .unwrap_err();
            assert!(matches!(err, QueryError::Fetch(_)));
        }

        let block = crate::GetBlock::get_block(&mock, height(0))
            .await
            .expect("succeeds after 3 failures");
        assert_eq!(block.header.hash, hash(1));
    }
}
