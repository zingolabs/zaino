//! In-memory validator for tests: a best chain (movable mid-test, for reorgs), failure injection
//!
//! - Answers like zebrad: by height *and* by hash from the best chain only (a side-chain block =
//!   not found), nothing above the tip
//! - Also a chain view endpoint: tip, readiness, an empty mempool, no peers

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::RwLock;

use zaino_primitives::types::{
    Block, BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, Height, PeerInfo,
    TransactionId, TransactionLocation,
};

use crate::{
    BlockLink, FailureMode, GetBlockByHashError, GetBlockError, GetBlockLinkError,
    GetChainTipError, GetMempoolListingError, GetPeerInfoError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, NonDomainError, QueryError, SendRawTransactionError,
    TransactionResponse,
};

pub struct MockChain {
    chain: RwLock<Chain>,
    /// `false` = zebrad still loading its state (`getbestblockheightandhash` = not ready)
    ready: AtomicBool,
    failures_remaining: AtomicU32,
    failure_mode: FailureMode,
}

/// Best chain by height, up to `tip`; `blocks` = every block ever added (orphans kept, unserved)
#[derive(Default)]
struct Chain {
    best: HashMap<Height, BlockHash>,
    blocks: HashMap<BlockHash, Block>,
    tip: Option<Height>,
}

impl Chain {
    /// `block` = the new tip (everything above its height leaves the best chain)
    fn put(&mut self, block: Block) {
        let height = block.header().height;
        self.best.retain(|held, _| *held < height);
        self.best.insert(height, block.header().hash);
        self.tip = Some(height);
        self.blocks.insert(block.header().hash, block);
    }

    fn best_at(&self, height: Height) -> Option<&Block> {
        self.best.get(&height).and_then(|hash| self.blocks.get(hash))
    }

    fn best_by_hash(&self, hash: BlockHash) -> Option<&Block> {
        let block = self.blocks.get(&hash)?;
        (self.best.get(&block.header().height) == Some(&hash)).then_some(block)
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
            ready: AtomicBool::new(true),
            failures_remaining: AtomicU32::new(0),
            failure_mode: FailureMode::Connection,
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    fn tip(&self) -> Option<(BlockHash, Height)> {
        let chain = self.chain.read().expect("mock chain lock");
        let height = chain.tip?;
        Some((*chain.best.get(&height)?, height))
    }

    /// Last block added = tip
    pub fn with_block(self, block: Block) -> Self {
        self.extend_best([block]);
        self
    }

    /// Puts `blocks` on the best chain in order, each becoming the tip (a reorg when a height was
    /// already held: that height and everything above it leave the best chain)
    pub fn extend_best(&self, blocks: impl IntoIterator<Item = Block>) {
        let mut chain = self.chain.write().expect("mock chain lock");
        for block in blocks {
            chain.put(block);
        }
    }

    /// Best chain cut back to `tip` (`invalidateblock` above it)
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
            .best_at(height)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl crate::GetBlockLink for MockChain {
    async fn get_block_link(
        &self,
        height: Height,
    ) -> Result<BlockLink, QueryError<GetBlockLinkError>> {
        self.injected()?;
        let chain = self.chain.read().expect("mock chain lock");
        chain
            .best_at(height)
            .map(|block| BlockLink {
                hash: block.header().hash,
                prev_hash: block.header().prev_hash,
            })
            .ok_or(QueryError::Domain(GetBlockLinkError::HeightNotFound(height)))
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
            .best_by_hash(hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl crate::GetBlockchainInfo for MockChain {
    /// Tip + hash only; estimate = the tip (no clock); no tip = genesis, as zebrad's own fallback
    ///
    /// - Schedule empty, branch 0, Sapling at genesis (no fabricated upgrade a test could pass on)
    async fn get_blockchain_info(&self) -> Result<BlockchainInfo, QueryError<Infallible>> {
        self.injected()?;
        let (hash, height) = self.tip().unwrap_or((BlockHash::ZERO, Height::GENESIS));
        let sprout = ConsensusBranchId::new(0);
        Ok(BlockchainInfo {
            blocks: height,
            estimated_height: height,
            best_block_hash: hash,
            sapling_activation: Height::GENESIS,
            upgrades: Vec::new(),
            consensus: ConsensusBranchIds { chain_tip: sprout, next_block: sprout },
        })
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

impl crate::GetChainTip for MockChain {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        self.injected()?;
        let ready = self.ready.load(Ordering::SeqCst);
        self.tip().filter(|_| ready).ok_or(QueryError::Domain(GetChainTipError::NotReady))
    }
}

impl crate::GetMempoolListing for MockChain {
    async fn get_mempool_listing(
        &self,
    ) -> Result<Vec<MempoolListed>, QueryError<GetMempoolListingError>> {
        self.injected()?;
        Ok(Vec::new())
    }
}

impl crate::GetRawMempoolTransaction for MockChain {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        self.injected()?;
        Err(QueryError::Domain(GetRawMempoolTransactionError::NotFound(txid)))
    }
}

impl crate::GetPeerInfo for MockChain {
    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>> {
        self.injected()?;
        Ok(Vec::new())
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
    use crate::{GetBlock, GetBlockByHash, GetBlockLink};

    /// Blocks and links answer by height and hash, a miss is the port's domain answer, and injected
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

        let link = chain.get_block_link(height(1)).await.expect("link 1");
        let expected =
            BlockLink { hash: BlockHash::from([2; 32]), prev_hash: BlockHash::from([1; 32]) };
        assert_eq!(link, expected);
        let missing_link = chain.get_block_link(height(9)).await;
        assert!(matches!(missing_link, Err(Domain(GetBlockLinkError::HeightNotFound(_)))));
    }
}
