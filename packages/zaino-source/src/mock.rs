//! In-memory mock adapter for testing.
//!
//! Implements the query traits against a pre-populated chain.
//! Supports failure injection for resilience testing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zaino_primitives::types::{Block, BlockHash, Height, TransactionId, Treestate};

use crate::error::{FailureMode, NonDomainError};
use crate::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    GetBlockByHashError, GetBlockError, GetChainTipError, GetSubtreeRootsError,
    GetTransactionError, GetTreestateError, QueryError, SendRawTransactionError,
    TransactionResponse,
};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, ShieldedPool, SubtreeRoot, TransactionLocation, Utxo,
};

/// A pre-populated in-memory chain for testing.
pub struct MockChain {
    blocks: HashMap<u32, Block>,
    by_hash: HashMap<[u8; 32], u32>,
    tip: Option<(BlockHash, Height)>,
    treestates: HashMap<u32, Treestate>,
    failures_remaining: AtomicU32,
    failure_mode: FailureMode,
    /// When set, `send_raw_transaction` rejects with this domain error; otherwise
    /// it accepts and echoes an id derived from the submitted bytes.
    send_rejection: Option<SendRawTransactionError>,
    /// When set, every address query rejects the addresses as invalid with this
    /// reason; otherwise they answer with empty (no-match) results.
    address_rejection: Option<String>,
    /// Canned raw-transaction response, returned for any txid; `None` answers a
    /// domain not-found.
    transaction_response: Option<TransactionResponse>,
    /// Canned subtree roots, returned for any index query.
    subtree_roots: Vec<SubtreeRoot>,
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
            send_rejection: None,
            address_rejection: None,
            transaction_response: None,
            subtree_roots: Vec::new(),
        }
    }

    /// Seed the response `get_transaction` returns for any txid.
    pub fn respond_transaction(mut self, bytes: Vec<u8>, location: TransactionLocation) -> Self {
        self.transaction_response = Some(TransactionResponse { bytes, location });
        self
    }

    /// Seed a subtree root `get_subtree_roots` returns.
    pub fn with_subtree_root(mut self, root: SubtreeRoot) -> Self {
        self.subtree_roots.push(root);
        self
    }

    /// Make `send_raw_transaction` reject with a domain error, for exercising a
    /// consumer's rejection path.
    pub fn reject_send(mut self, err: SendRawTransactionError) -> Self {
        self.send_rejection = Some(err);
        self
    }

    /// Make every address query reject its addresses as invalid, for exercising
    /// a consumer's domain-rejection path on the address reads.
    pub fn reject_addresses(mut self, reason: impl Into<String>) -> Self {
        self.address_rejection = Some(reason.into());
        self
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
            Ok(_) => Some(QueryError::NonDomain(NonDomainError::new(
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

// Manual because `AtomicU32` is not `Clone`; the remaining-failures count is
// copied by value. Consumers that need a cloneable resilient handle (the engine
// captures its source into each snapshot) wrap the mock in a `ValidatorClient`,
// which is `Clone` only when its inner source is.
impl Clone for MockChain {
    fn clone(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            by_hash: self.by_hash.clone(),
            tip: self.tip,
            treestates: self.treestates.clone(),
            failures_remaining: AtomicU32::new(self.failures_remaining.load(Ordering::SeqCst)),
            failure_mode: self.failure_mode.clone(),
            send_rejection: self.send_rejection.clone(),
            address_rejection: self.address_rejection.clone(),
            transaction_response: self.transaction_response.clone(),
            subtree_roots: self.subtree_roots.clone(),
        }
    }
}

impl crate::ValidatorSource for MockChain {
    type NonDomain = crate::NonDomainError;
}

impl crate::OneShotGetBlock for MockChain {
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

impl crate::OneShotGetBlockByHash for MockChain {
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

impl crate::OneShotGetChainTip for MockChain {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.tip
            .ok_or(QueryError::Domain(GetChainTipError::NotReady))
    }
}

impl crate::OneShotGetTreestate for MockChain {
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

// A static mock does not push tip updates; the default `None` says "no
// subscription", so a consumer bound on `SubscribeChainTip` still accepts it
// (and simply does not tip-follow).
impl crate::SubscribeChainTip for MockChain {}

impl crate::OneShotSendRawTransaction for MockChain {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        if let Some(rejection) = &self.send_rejection {
            return Err(QueryError::Domain(rejection.clone()));
        }
        // Accept: echo a deterministic id from the submitted bytes so a test can
        // assert the exact transaction was relayed.
        let mut id = [0u8; 32];
        let taken = transaction.len().min(32);
        id[..taken].copy_from_slice(&transaction[..taken]);
        Ok(TransactionId::from(id))
    }
}

impl crate::OneShotGetMempoolTxids for MockChain {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<crate::GetMempoolTxidsError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        // The static mock carries no mempool: an empty listing is the honest
        // answer, not "unavailable" (which would tell a consumer to stop asking).
        Ok(Vec::new())
    }
}

impl crate::OneShotGetRawMempoolTransaction for MockChain {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<crate::GetRawMempoolTransactionError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        Err(QueryError::Domain(
            crate::GetRawMempoolTransactionError::NotFound(txid),
        ))
    }
}

impl crate::OneShotGetMempoolSourceTip for MockChain {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<std::convert::Infallible>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        // This port carries no domain error, so a mock with no tip reports a
        // transport failure — the only non-success it can return.
        self.tip.ok_or_else(|| {
            QueryError::NonDomain(NonDomainError::new(
                FailureMode::Connection,
                "mock has no tip".to_string(),
            ))
        })
    }
}

impl crate::OneShotGetAddressBalance for MockChain {
    async fn get_address_balance(
        &self,
        _addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        if let Some(reason) = &self.address_rejection {
            return Err(QueryError::Domain(GetAddressBalanceError::InvalidAddress(
                reason.clone(),
            )));
        }
        Ok(AddressBalance {
            balance: zaino_primitives::types::Zatoshis::ZERO,
            received: zaino_primitives::types::ZatoshisFlowSum::from_summed(0),
        })
    }
}

impl crate::OneShotGetAddressUtxos for MockChain {
    async fn get_address_utxos(
        &self,
        _addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        if let Some(reason) = &self.address_rejection {
            return Err(QueryError::Domain(GetAddressUtxosError::InvalidAddress(
                reason.clone(),
            )));
        }
        Ok(Vec::new())
    }
}

impl crate::OneShotGetAddressTxids for MockChain {
    async fn get_address_txids(
        &self,
        _addresses: Vec<String>,
        _start: Height,
        _end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        if let Some(reason) = &self.address_rejection {
            return Err(QueryError::Domain(GetAddressTxidsError::InvalidAddress(
                reason.clone(),
            )));
        }
        Ok(Vec::new())
    }
}

impl crate::OneShotGetAddressDeltas for MockChain {
    async fn get_address_deltas(
        &self,
        _addresses: Vec<String>,
        _start: Height,
        _end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        if let Some(reason) = &self.address_rejection {
            return Err(QueryError::Domain(GetAddressDeltasError::InvalidAddress(
                reason.clone(),
            )));
        }
        Ok(Vec::new())
    }
}

impl crate::OneShotGetTransaction for MockChain {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.transaction_response
            .clone()
            .ok_or(QueryError::Domain(GetTransactionError::NotFound(txid)))
    }
}

impl crate::OneShotGetSubtreeRoots for MockChain {
    async fn get_subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        Ok(self.subtree_roots.clone())
    }
}

/// A minimal test [`Block`] at `height` with hash `[hash_byte; 32]`, for seeding
/// a [`MockChain`] (or other source fixtures) from downstream crates. Behind the
/// `testing` feature so it is reusable, not just an in-crate test helper.
#[cfg(any(test, feature = "testing"))]
pub fn test_block(height: u32, hash_byte: u8) -> Block {
    use zaino_primitives::types::{
        BlockHeader, ChainMetadata, CompactDifficulty, EquihashSolution,
    };
    Block {
        header: BlockHeader {
            hash: BlockHash::from([hash_byte; 32]),
            version: 4,
            prev_hash: BlockHash::ZERO,
            height: Height::try_from(height).expect("valid test height"),
            time: 0,
            merkle_root: [0; 32].into(),
            block_commitments: [0; 32].into(),
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: vec![],
        chain_metadata: ChainMetadata::ZERO,
    }
}

/// A hash-linked [`MockChain`] of `len` blocks at heights `0..len`: each block's
/// `prev_hash` points at its predecessor's hash, and genesis links to
/// [`BlockHash::ZERO`]. Unlike seeding isolated [`test_block`]s, this yields a
/// well-formed chain — the shape the conformance battery checks. Distinct hashes
/// hold for `len <= 200`; longer chains repeat and are not intended.
#[cfg(any(test, feature = "testing"))]
pub fn linked_test_chain(len: u32) -> MockChain {
    let mut chain = MockChain::new();
    let mut prev = BlockHash::ZERO;
    for h in 0..len {
        let hash_byte = u8::try_from(h % 200 + 1).expect("fits in u8 for len <= 200");
        let mut block = test_block(h, hash_byte);
        block.header.prev_hash = prev;
        prev = block.header.hash;
        chain = chain.with_block(block);
    }
    chain
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
        let err = crate::OneShotGetChainTip::get_chain_tip(&mock)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetChainTipError::NotReady)
        ));
    }

    #[tokio::test]
    async fn get_block_roundtrip() {
        let mock = MockChain::new().with_block(test_block(0, 1));
        let block = crate::OneShotGetBlock::get_block(&mock, height(0))
            .await
            .expect("block exists");
        assert_eq!(block.header.hash, hash(1));
    }

    #[tokio::test]
    async fn get_block_not_found() {
        let mock = MockChain::new();
        let err = crate::OneShotGetBlock::get_block(&mock, height(99))
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
        let block = crate::OneShotGetBlockByHash::get_block_by_hash(&mock, hash(1))
            .await
            .expect("block exists");
        assert_eq!(block.header.height, height(0));
    }

    #[tokio::test]
    async fn tip_is_last_added_block() {
        let mock = MockChain::new()
            .with_block(test_block(0, 1))
            .with_block(test_block(1, 2));
        let (tip_hash, tip_height) = crate::OneShotGetChainTip::get_chain_tip(&mock)
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
        let result = crate::OneShotGetTreestate::get_treestate(&mock, height(0))
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

        let err = crate::OneShotGetBlock::get_block(&mock, height(0))
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::NonDomain(ref e) if e.mode == FailureMode::Timeout));

        let block = crate::OneShotGetBlock::get_block(&mock, height(0))
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
            let err = crate::OneShotGetBlock::get_block(&mock, height(0))
                .await
                .unwrap_err();
            assert!(matches!(err, QueryError::NonDomain(_)));
        }

        let block = crate::OneShotGetBlock::get_block(&mock, height(0))
            .await
            .expect("succeeds after 3 failures");
        assert_eq!(block.header.hash, hash(1));
    }
}
