//! In-memory mock adapter for testing.
//!
//! Implements the query traits against a pre-populated chain.
//! No network, no disk — just a `HashMap`.
//!
//! ```ignore
//! let mock = MockChain::new()
//!     .with_block(height, hash, bytes)
//!     .with_block(height, hash, bytes);
//! let tip = mock.get_chain_tip().await?;
//! ```

use std::collections::HashMap;

use zaino_primitives::types::{BlockHash, Height, Treestate};

use crate::{
    GetBlockBytesError, GetBlockByHashError, GetChainTipError, GetTreestateError, QueryError,
};

/// A pre-populated in-memory chain for testing.
pub struct MockChain {
    blocks: HashMap<u32, MockBlock>,
    by_hash: HashMap<[u8; 32], u32>,
    tip: Option<(BlockHash, Height)>,
    treestates: HashMap<u32, Treestate>,
}

struct MockBlock {
    bytes: Vec<u8>,
}

impl MockChain {
    /// Empty chain.
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            by_hash: HashMap::new(),
            tip: None,
            treestates: HashMap::new(),
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
        self.blocks
            .get(&u32::from(height))
            .map(|b| b.bytes.clone())
            .ok_or(QueryError::Domain(GetBlockBytesError::HeightNotFound(height)))
    }
}

impl crate::GetBlockByHash for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
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
    async fn get_chain_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        self.tip
            .ok_or(QueryError::Domain(GetChainTipError::NotReady))
    }
}

impl crate::GetTreestate for MockChain {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        self.treestates
            .get(&u32::from(height))
            .cloned()
            .ok_or(QueryError::Domain(GetTreestateError::HeightNotFound(height)))
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
        assert!(matches!(err, QueryError::Domain(GetChainTipError::NotReady)));
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
}
