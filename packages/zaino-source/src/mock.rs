//! In-memory mock adapter for testing.
//!
//! Implements the query traits against a pre-populated chain.
//! Supports failure injection for resilience testing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zaino_primitives::types::{Block, BlockHash, Height, TransactionId, Treestate};

use crate::error::{FailureMode, NonDomainError};
use crate::{
    DecodedTransaction, GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError,
    GetAddressUtxosError, GetBlockByHashError, GetBlockError, GetBlockHeaderError,
    GetBlockVerboseError, GetBlockchainInfoError, GetChainTipError, GetSubtreeRootsError,
    GetTransactionError, GetTransactionVerboseError, GetTreestateError, QueryError,
    SendRawTransactionError, TransactionResponse,
};
use zaino_primitives::types::rpc::BlockHeaderVerbose;
use zaino_primitives::types::{
    AddressBalance, AddressDelta, BlockVerbose, BlockchainInfo, ShieldedPool, SubtreeRoot,
    Transaction, TransactionDetail, TransactionLocation, Utxo,
};

/// The default detail a seeded verbose response carries: a v5, non-coinbase
/// transaction with no Sprout movement and no expiry.
fn default_verbose_detail() -> TransactionDetail {
    TransactionDetail {
        version: 5,
        overwintered: true,
        // TX_V5_VERSION_GROUP_ID.
        version_group_id: Some(0x26A7_270A),
        lock_time: 0,
        expiry_height: Some(Height::GENESIS),
        size: 0,
        coinbase: None,
        joinsplits: Vec::new(),
    }
}

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
    /// Canned decoded-transaction response, returned for any txid; `None` answers
    /// a domain not-found.
    transaction_verbose_response: Option<DecodedTransaction>,
    /// Canned blockchain-info response, returned for any query; `None` answers a
    /// domain not-ready.
    blockchain_info_response: Option<BlockchainInfo>,
    /// Canned verbose block-header response, returned for any hash; `None`
    /// answers a domain not-found.
    block_header_verbose_response: Option<BlockHeaderVerbose>,
    /// Canned verbose block response, returned for any height or hash; `None`
    /// answers a domain not-found.
    block_verbose_response: Option<BlockVerbose>,
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
            transaction_verbose_response: None,
            blockchain_info_response: None,
            block_header_verbose_response: None,
            block_verbose_response: None,
            subtree_roots: Vec::new(),
        }
    }

    /// Seed the response `get_transaction` returns for any txid.
    pub fn respond_transaction(mut self, bytes: Vec<u8>, location: TransactionLocation) -> Self {
        self.transaction_response = Some(TransactionResponse { bytes, location });
        self
    }

    /// Seed the response `get_transaction_verbose` returns for any txid.
    ///
    /// The [`detail`](DecodedTransaction::detail) defaults to a v5 non-coinbase
    /// envelope; override it with [`with_detail`](Self::with_detail).
    pub fn respond_transaction_verbose(
        mut self,
        transaction: Transaction,
        location: TransactionLocation,
    ) -> Self {
        self.transaction_verbose_response = Some(DecodedTransaction {
            transaction,
            detail: default_verbose_detail(),
            location,
        });
        self
    }

    /// Override the detail of the seeded `get_transaction_verbose` response.
    ///
    /// Call after [`respond_transaction_verbose`](Self::respond_transaction_verbose);
    /// with no seeded response it is a no-op.
    pub fn with_detail(mut self, detail: TransactionDetail) -> Self {
        if let Some(response) = self.transaction_verbose_response.as_mut() {
            response.detail = detail;
        }
        self
    }

    /// Seed the response `get_blockchain_info` returns.
    pub fn with_blockchain_info(mut self, info: BlockchainInfo) -> Self {
        self.blockchain_info_response = Some(info);
        self
    }

    /// Seed the response `get_block_header` returns for any hash.
    pub fn with_block_header_verbose(mut self, header: BlockHeaderVerbose) -> Self {
        self.block_header_verbose_response = Some(header);
        self
    }

    /// Seed the response `get_block_verbose` / `get_block_verbose_by_hash`
    /// returns for any height or hash.
    pub fn with_block_verbose(mut self, block: BlockVerbose) -> Self {
        self.block_verbose_response = Some(block);
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
            transaction_verbose_response: self.transaction_verbose_response.clone(),
            blockchain_info_response: self.blockchain_info_response.clone(),
            block_header_verbose_response: self.block_header_verbose_response.clone(),
            block_verbose_response: self.block_verbose_response.clone(),
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

impl crate::OneShotGetMempoolCompactTransaction for MockChain {
    async fn get_mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<
        zaino_primitives::types::PreIndexCompactTx,
        QueryError<crate::GetRawMempoolTransactionError>,
    > {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        // The static mock carries no mempool transactions.
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

impl crate::OneShotGetTransactionVerbose for MockChain {
    async fn get_transaction_verbose(
        &self,
        txid: TransactionId,
    ) -> Result<DecodedTransaction, QueryError<GetTransactionVerboseError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.transaction_verbose_response
            .clone()
            .ok_or(QueryError::Domain(GetTransactionVerboseError::NotFound(
                txid,
            )))
    }
}

impl crate::OneShotGetBlockchainInfo for MockChain {
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.blockchain_info_response
            .clone()
            .ok_or(QueryError::Domain(GetBlockchainInfoError::NotReady))
    }
}

impl crate::OneShotGetBlockHeader for MockChain {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<BlockHeaderVerbose, QueryError<GetBlockHeaderError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.block_header_verbose_response
            .clone()
            .ok_or(QueryError::Domain(GetBlockHeaderError::BlockNotFound(hash)))
    }
}

impl crate::OneShotGetBlockVerbose for MockChain {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.block_verbose_response
            .clone()
            .ok_or(QueryError::Domain(GetBlockVerboseError::HeightNotFound(
                height,
            )))
    }
}

impl crate::OneShotGetBlockVerboseByHash for MockChain {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.block_verbose_response
            .clone()
            .ok_or(QueryError::Domain(GetBlockVerboseError::BlockNotFound(
                hash,
            )))
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

/// A [`BlockchainInfo`] fixture carrying a distinct, non-default value in every
/// field, for seeding a [`MockChain`] (or asserting a passthrough carried each
/// field intact) from downstream crates. Behind the `testing` feature so it is
/// reusable, not just an in-crate test helper. The values are arbitrary but
/// mutually distinguishable, so a consumer that drops or defaults any one field
/// fails an equality check against this fixture.
#[cfg(any(test, feature = "testing"))]
pub fn sample_blockchain_info() -> BlockchainInfo {
    use zaino_primitives::types::{
        BlockHash, ConsensusBranchId, ConsensusBranchIds, Height, ValuePoolBalance, Zatoshis,
    };
    BlockchainInfo {
        chain: "sample-chain".to_string(),
        blocks: Height::try_from(111).expect("valid height"),
        headers: Height::try_from(222).expect("valid height"),
        estimated_height: Height::try_from(333).expect("valid height"),
        best_block_hash: BlockHash::from([0xab; 32]),
        difficulty: 44.5,
        verification_progress: 0.75,
        chain_work: None,
        pruned: true,
        size_on_disk: 555,
        commitments: 666,
        chain_supply: ValuePoolBalance {
            id: "supply".to_string(),
            chain_value: Zatoshis::new(7000).expect("valid amount"),
            monitored: true,
            value_delta: None,
        },
        value_pools: vec![ValuePoolBalance {
            id: "orchard".to_string(),
            chain_value: Zatoshis::new(8000).expect("valid amount"),
            monitored: true,
            value_delta: None,
        }],
        upgrades: Vec::new(),
        consensus: ConsensusBranchIds {
            chain_tip: ConsensusBranchId::new(0x1234),
            next_block: ConsensusBranchId::new(0x5678),
        },
    }
}

/// A [`BlockHeaderVerbose`] fixture with a distinct, non-default value in every
/// field, for seeding a [`MockChain`] (or asserting a passthrough carried each
/// field intact) from downstream crates. Behind the `testing` feature so it is
/// reusable, not just an in-crate test helper. The values are arbitrary but
/// mutually distinguishable, so a consumer that drops or defaults any one field
/// fails an equality check against this fixture.
#[cfg(any(test, feature = "testing"))]
pub fn sample_block_header_verbose() -> BlockHeaderVerbose {
    use zaino_primitives::types::{AbsoluteChainWork, BlockHash, CompactDifficulty, Height};
    let mut work_bytes = [0u8; 32];
    work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    BlockHeaderVerbose {
        hash: BlockHash::from([0x11; 32]),
        confirmations: 12,
        height: Height::try_from(654_321).expect("valid height"),
        version: 4,
        merkle_root: [0x22; 32].into(),
        time: 1_600_000_000,
        nonce: [0x33; 32],
        solution: vec![0x44, 0x45, 0x46],
        bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
        difficulty: 1.5,
        block_commitments: Some([0x55; 32].into()),
        final_sapling_root: Some([0x66; 32].into()),
        chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
        previous_block_hash: Some(BlockHash::from([0x77; 32])),
        next_block_hash: Some(BlockHash::from([0x88; 32])),
    }
}

/// A [`BlockVerbose`] fixture with a distinct, non-default value in every field,
/// for seeding a [`MockChain`] (or asserting a passthrough carried each field
/// intact) from downstream crates. Behind the `testing` feature so it is
/// reusable, not just an in-crate test helper. The values are arbitrary but
/// mutually distinguishable, so a consumer that drops or defaults any one field
/// fails an equality check against this fixture.
#[cfg(any(test, feature = "testing"))]
pub fn sample_block_verbose() -> BlockVerbose {
    use zaino_primitives::types::{
        AbsoluteChainWork, BlockHash, BlockTreeSizes, TreeSize, ValuePoolBalance, Zatoshis,
    };
    let mut work_bytes = [0u8; 32];
    work_bytes[28..].copy_from_slice(&[0x0b, 0xad, 0xf0, 0x0d]);
    BlockVerbose {
        confirmations: 9,
        difficulty: 2.5,
        chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
        chain_supply: Some(ValuePoolBalance {
            id: String::new(),
            chain_value: Zatoshis::new(21_000_000).expect("valid amount"),
            monitored: true,
            value_delta: None,
        }),
        value_pools: vec![ValuePoolBalance {
            id: "orchard".to_string(),
            chain_value: Zatoshis::new(3_000).expect("valid amount"),
            monitored: true,
            value_delta: None,
        }],
        tree_sizes: BlockTreeSizes {
            sapling: TreeSize::from(10u32),
            orchard: TreeSize::from(20u32),
            ironwood: TreeSize::from(30u32),
        },
        next_block_hash: Some(BlockHash::from([0x99; 32])),
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

    fn empty_transaction(txid_byte: u8) -> Transaction {
        Transaction {
            txid: TransactionId::from([txid_byte; 32]),
            transparent: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    #[tokio::test]
    async fn get_transaction_verbose_returns_the_scripted_transaction() {
        let mock = MockChain::new().respond_transaction_verbose(
            empty_transaction(7),
            TransactionLocation::BestChain(height(5)),
        );
        let decoded = crate::OneShotGetTransactionVerbose::get_transaction_verbose(
            &mock,
            TransactionId::from([9u8; 32]),
        )
        .await
        .expect("scripted transaction");
        assert_eq!(decoded.transaction.txid, TransactionId::from([7u8; 32]));
        assert_eq!(decoded.location, TransactionLocation::BestChain(height(5)));
    }

    #[tokio::test]
    async fn get_transaction_verbose_not_found() {
        let mock = MockChain::new();
        let txid = TransactionId::from([3u8; 32]);
        let err = crate::OneShotGetTransactionVerbose::get_transaction_verbose(&mock, txid)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetTransactionVerboseError::NotFound(got)) if got == txid
        ));
    }

    #[tokio::test]
    async fn get_blockchain_info_returns_the_scripted_info() {
        let mock = MockChain::new().with_blockchain_info(sample_blockchain_info());
        let info = crate::OneShotGetBlockchainInfo::get_blockchain_info(&mock)
            .await
            .expect("scripted info");
        assert_eq!(info, sample_blockchain_info());
    }

    #[tokio::test]
    async fn get_blockchain_info_not_ready_without_a_script() {
        let mock = MockChain::new();
        let err = crate::OneShotGetBlockchainInfo::get_blockchain_info(&mock)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockchainInfoError::NotReady)
        ));
    }

    #[tokio::test]
    async fn get_block_header_returns_the_scripted_header() {
        let mock = MockChain::new().with_block_header_verbose(sample_block_header_verbose());
        let header = crate::OneShotGetBlockHeader::get_block_header(&mock, hash(9))
            .await
            .expect("scripted header");
        assert_eq!(header, sample_block_header_verbose());
    }

    #[tokio::test]
    async fn get_block_header_not_found_without_a_script() {
        let mock = MockChain::new();
        let err = crate::OneShotGetBlockHeader::get_block_header(&mock, hash(3))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockHeaderError::BlockNotFound(got)) if got == hash(3)
        ));
    }

    #[tokio::test]
    async fn get_block_verbose_returns_the_scripted_block() {
        let mock = MockChain::new().with_block_verbose(sample_block_verbose());
        let block = crate::OneShotGetBlockVerbose::get_block_verbose(&mock, height(5))
            .await
            .expect("scripted block");
        assert_eq!(block, sample_block_verbose());
    }

    #[tokio::test]
    async fn get_block_verbose_not_found_without_a_script() {
        let mock = MockChain::new();
        let err = crate::OneShotGetBlockVerbose::get_block_verbose(&mock, height(5))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockVerboseError::HeightNotFound(got)) if got == height(5)
        ));
    }

    #[tokio::test]
    async fn get_block_verbose_by_hash_returns_the_scripted_block() {
        let mock = MockChain::new().with_block_verbose(sample_block_verbose());
        let block = crate::OneShotGetBlockVerboseByHash::get_block_verbose_by_hash(&mock, hash(5))
            .await
            .expect("scripted block");
        assert_eq!(block, sample_block_verbose());
    }

    #[tokio::test]
    async fn get_block_verbose_by_hash_not_found_without_a_script() {
        let mock = MockChain::new();
        let err = crate::OneShotGetBlockVerboseByHash::get_block_verbose_by_hash(&mock, hash(7))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::Domain(GetBlockVerboseError::BlockNotFound(got)) if got == hash(7)
        ));
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
