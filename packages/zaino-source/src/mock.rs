//! In-memory validator for tests: a best chain (movable mid-test, for reorgs), every block it ever
//! held (by hash), failure injection

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

use zaino_primitives::types::{
    Block, BlockHash, BlockchainInfo, Height, TransactionId, TransactionLocation,
};

use crate::{
    FailureMode, GetBlockByHashError, GetBlockError, GetBlockchainInfoError, GetTransactionError,
    NonDomainError, QueryError, SendRawTransactionError, TransactionResponse,
};

pub struct MockChain {
    chain: RwLock<Chain>,
    failures_remaining: AtomicU32,
    failure_mode: FailureMode,
}

/// Best chain by height; every block ever added by hash (a reorged-out block still resolves)
#[derive(Default)]
struct Chain {
    best: HashMap<Height, BlockHash>,
    blocks: HashMap<BlockHash, Block>,
    tip: Option<Height>,
}

impl Chain {
    fn put(&mut self, block: Block) {
        let height = block.header().height;
        self.best.insert(height, block.header().hash);
        self.tip = Some(height);
        self.blocks.insert(block.header().hash, block);
    }
}

impl Default for MockChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChain {
    pub fn new() -> Self {
        Self {
            chain: RwLock::new(Chain::default()),
            failures_remaining: AtomicU32::new(0),
            failure_mode: FailureMode::Connection,
        }
    }

    /// Last block added = tip
    pub fn with_block(self, block: Block) -> Self {
        self.extend_best([block]);
        self
    }

    /// Puts `blocks` on the best chain in order, each replacing whatever held its height; the
    /// last one becomes the tip (a reorg when a height was already held)
    pub fn extend_best(&self, blocks: impl IntoIterator<Item = Block>) {
        let mut chain = self.chain.write().expect("mock chain lock");
        for block in blocks {
            chain.put(block);
        }
    }

    /// Best chain cut back to `tip` (`invalidateblock` above it); orphans still found by hash
    pub fn rewind_to(&self, tip: Height) {
        let mut chain = self.chain.write().expect("mock chain lock");
        chain.best.retain(|height, _| *height <= tip);
        chain.tip = Some(tip);
    }

    /// Next `count` calls fail with `mode` (any port)
    pub fn fail_next(self, count: u32, mode: FailureMode) -> Self {
        self.failures_remaining.store(count, Ordering::SeqCst);
        Self { failure_mode: mode, ..self }
    }

    fn injected<E: core::fmt::Debug + core::fmt::Display>(&self) -> Result<(), QueryError<E>> {
        match self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        {
            Ok(_) => Err(QueryError::NonDomain(NonDomainError::new(
                self.failure_mode.clone(),
                format!("mock injected {:?}", self.failure_mode),
            ))),
            Err(_) => Ok(()),
        }
    }
}

impl crate::GetBlock for MockChain {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.injected()?;
        let chain = self.chain.read().expect("mock chain lock");
        chain
            .best
            .get(&height)
            .and_then(|hash| chain.blocks.get(hash))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl crate::GetBlockByHash for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.injected()?;
        self.chain
            .read()
            .expect("mock chain lock")
            .blocks
            .get(&hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl crate::GetBlockchainInfo for MockChain {
    /// Never a fabricated chain description (a test would pass on numbers no node produced)
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        Err(QueryError::Domain(GetBlockchainInfoError::NotReady))
    }
}

impl crate::SendRawTransaction for MockChain {
    /// Accepted and dropped: txid = the first 32 bytes (no relay to assert on)
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        self.injected()?;
        let mut txid = [0u8; 32];
        let taken = transaction.len().min(32);
        txid[..taken].copy_from_slice(&transaction[..taken]);
        Ok(TransactionId::from(txid))
    }
}

impl crate::GetTransaction for MockChain {
    /// Txid echoed as the body, placed at the tip (wiring tests assert the forward, not contents)
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.injected()?;
        Ok(TransactionResponse {
            bytes: <[u8; 32]>::from(txid).to_vec(),
            location: match self.chain.read().expect("mock chain lock").tip {
                Some(height) => TransactionLocation::BestChain(height),
                None => TransactionLocation::Mempool,
            },
        })
    }
}

/// Block `height` with hash `[hash_byte; 32]`, linked onto `test_block(_, hash_byte − 1)`
/// - bare coinbase with txid `[hash_byte; 32]`
pub fn test_block(height: u32, hash_byte: u8) -> Block {
    use zaino_primitives::types::{BlockHeader, Transaction};
    Block::new(
        BlockHeader::for_tests(height, [hash_byte; 32], [hash_byte.wrapping_sub(1); 32], 0),
        vec![Transaction {
            txid: [hash_byte; 32].into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GetBlock, GetBlockByHash};

    /// Blocks answer by height and hash, a miss is the port's domain answer, and injected
    /// failures precede the real answer exactly `count` times
    #[tokio::test]
    async fn serves_blocks_by_height_and_hash_after_injected_failures() {
        let height = |h: u32| Height::try_from(h).expect("h");
        let chain = MockChain::new()
            .with_block(test_block(0, 1))
            .with_block(test_block(1, 2))
            .fail_next(2, FailureMode::Timeout);

        use QueryError::NonDomain;
        for _ in 0..2 {
            let failed = chain.get_block(height(1)).await;
            assert!(matches!(failed, Err(NonDomain(e)) if e.mode == FailureMode::Timeout));
        }
        let by_height = chain.get_block(height(1)).await.expect("height 1");
        let by_hash = chain.get_block_by_hash(BlockHash::from([1; 32])).await.expect("hash 1");
        let (by_height, by_hash) = (by_height.header(), by_hash.header());
        assert_eq!((by_height.hash, by_hash.height), (BlockHash::from([2; 32]), height(0)));
        let (missing_height, missing_hash) = (
            chain.get_block(height(9)).await,
            chain.get_block_by_hash(BlockHash::from([9; 32])).await,
        );
        use {GetBlockByHashError::NotFound, GetBlockError::HeightNotFound, QueryError::Domain};
        assert!(matches!(missing_height, Err(Domain(HeightNotFound(h))) if h == height(9)));
        assert!(matches!(missing_hash, Err(Domain(NotFound(_)))));
    }
}
