//! In-memory validator for tests over `zaino_primitives::testing::Chain` blocks: a best chain
//! (movable mid-test, for reorgs), a mempool, failure injection
//!
//! - Answers like zebrad: by height *and* by hash from the best chain only (a side-chain block =
//!   not found), nothing above the tip; links = each block's real header bytes
//! - Mempool listed at [`MEMPOOL_FEE`]; an accepted send listed from the next poll; a mined txid
//!   leaves it

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::RwLock;

use zaino_primitives::testing::{encode_header, header_hash};
use zaino_primitives::types::{
    Block, BlockHash, BlockRef, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds,
    EndOfService, Height, NodeRelease, TransactionId, TransactionLocation, Zatoshis,
};

use crate::{
    BlockLink, FailureMode, GetAtHeightError, GetBlockByHashError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, MetadataReading, NonDomainError, PollReading, QueryError,
    SendRawTransactionError, TransactionResponse,
};

pub const MEMPOOL_FEE: u64 = 1_000;

pub struct MockChain {
    held: RwLock<Held>,
    reachable: AtomicBool,
    failures_remaining: AtomicU32,
    failure_mode: FailureMode,
}

/// `blocks` = every block ever added (orphans kept, unserved)
#[derive(Default)]
struct Held {
    best: BTreeMap<Height, BlockHash>,
    blocks: HashMap<BlockHash, Block>,
    mempool: BTreeMap<TransactionId, Vec<u8>>,
}

impl Held {
    /// `block` = the new tip (everything at or above its height leaves the best chain)
    fn put(&mut self, block: Block) {
        let header = block.header();
        assert_eq!(header_hash(header), header.hash, "mock serves real headers (testing::Chain)");
        if let Some(below) = header.height.checked_sub(1).and_then(|h| self.best.get(&h)) {
            assert_eq!(header.prev_hash, *below, "best chain linked at {:?}", header.height);
        }
        self.best.split_off(&header.height);
        self.best.insert(header.height, header.hash);
        for tx in block.transactions() {
            self.mempool.remove(&tx.txid);
        }
        self.blocks.insert(header.hash, block);
    }

    fn tip(&self) -> Option<BlockRef> {
        self.best.last_key_value().map(|(&height, &hash)| BlockRef { hash, height })
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
            held: RwLock::new(Held::default()),
            reachable: AtomicBool::new(true),
            failures_remaining: AtomicU32::new(0),
            failure_mode: FailureMode::Connection,
        }
    }

    /// Best chain = `blocks` in order (`Chain::path`: genesis ..= tip)
    pub fn serving(blocks: impl IntoIterator<Item = Block>) -> Self {
        let mock = Self::new();
        mock.extend_best(blocks);
        mock
    }

    /// `false` = every call fails in transport until set back (a validator gone, then back)
    pub fn set_reachable(&self, reachable: bool) {
        self.reachable.store(reachable, Ordering::SeqCst);
    }

    /// `blocks` onto the best chain in order, each the new tip (height already held = reorg: it
    /// and everything above leave the best chain)
    pub fn extend_best(&self, blocks: impl IntoIterator<Item = Block>) {
        let mut held = self.held.write().expect("mock chain lock");
        for block in blocks {
            held.put(block);
        }
    }

    /// Listed from the next poll, `raw` served for it
    pub fn mempool_insert(&self, txid: TransactionId, raw: Vec<u8>) {
        self.held.write().expect("mock chain lock").mempool.insert(txid, raw);
    }

    fn injected<E: core::fmt::Debug + core::fmt::Display>(&self) -> Result<(), QueryError<E>> {
        self.injected_cause().map_err(QueryError::NonDomain)
    }

    fn injected_cause(&self) -> Result<(), NonDomainError> {
        if !self.reachable.load(Ordering::SeqCst) {
            return Err(NonDomainError::new(FailureMode::Connection, "mock unreachable"));
        }
        match self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        {
            Ok(_) => Err(NonDomainError::new(
                self.failure_mode.clone(),
                format!("mock injected {:?}", self.failure_mode),
            )),
            Err(_) => Ok(()),
        }
    }

    /// Tip + hash only; estimate = the tip (no clock); no tip = genesis, as zebrad's own fallback
    ///
    /// - Schedule empty, branch 0, Sapling at genesis (no fabricated upgrade a test could pass on)
    fn info(held: &Held) -> BlockchainInfo {
        let tip = held.tip().unwrap_or(BlockRef { hash: BlockHash::ZERO, height: Height::GENESIS });
        let sprout = ConsensusBranchId::new(0);
        BlockchainInfo {
            blocks: tip.height,
            estimated_height: tip.height,
            best_block_hash: tip.hash,
            sapling_activation: Height::GENESIS,
            upgrades: Vec::new(),
            consensus: ConsensusBranchIds { chain_tip: sprout, next_block: sprout },
        }
    }
}

/// Its chain and mempool; no peers, a release with no halt
impl crate::ChainDataSource for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.injected()?;
        let held = self.held.read().expect("mock chain lock");
        held.best_by_hash(hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }

    /// Txid from the bytes (a malformed tx = `Malformed`), listed from the next poll
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        self.injected()?;
        let prepared = crate::prepare_transaction(&transaction).map_err(|malformed| {
            QueryError::Domain(SendRawTransactionError::Malformed(malformed.to_string()))
        })?;
        self.mempool_insert(prepared.txid, transaction);
        Ok(prepared.txid)
    }

    /// Mempool: its bytes; mined on the best chain: its height, the txid's bytes as its body
    /// (blocks hold no transaction bytes)
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.injected()?;
        let held = self.held.read().expect("mock chain lock");
        if let Some(raw) = held.mempool.get(&txid) {
            return Ok(TransactionResponse {
                bytes: raw.clone(),
                location: TransactionLocation::Mempool,
            });
        }
        let mined = held
            .best
            .iter()
            .find(|(_, hash)| held.blocks[*hash].transactions().iter().any(|tx| tx.txid == txid));
        let (height, _) = mined.ok_or(QueryError::Domain(GetTransactionError::NotFound(txid)))?;
        Ok(TransactionResponse {
            bytes: <[u8; 32]>::from(txid).to_vec(),
            location: TransactionLocation::BestChain(*height),
        })
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.injected_cause()?;
        let held = self.held.read().expect("mock chain lock");
        let answers = holds.iter().map(|height| {
            let hash = held.best.get(height).copied();
            hash.ok_or(QueryError::Domain(GetAtHeightError::HeightNotFound(*height)))
        });
        let fee = Zatoshis::new(MEMPOOL_FEE).expect("in supply");
        let listing = held.mempool.iter().map(|(txid, raw)| MempoolListed {
            txid: *txid,
            fee,
            encoded_len: u32::try_from(raw.len()).expect("test transactions under 4 GiB"),
        });
        let release = NodeRelease {
            build: "mock".to_owned(),
            user_agent: "/MockChain/".to_owned(),
            protocol_version: 0,
            end_of_service: EndOfService::NotEnforced,
        };
        Ok(PollReading {
            info: Self::info(&held),
            listing: Ok(listing.collect()),
            held: answers.collect(),
            metadata: metadata
                .then(|| MetadataReading { peers: Ok(Vec::new()), release: Ok(release) }),
        })
    }

    async fn get_block_links(
        &self,
        heights: &[Height],
    ) -> Result<crate::BlockLinks, NonDomainError> {
        self.injected_cause()?;
        let held = self.held.read().expect("mock chain lock");
        Ok(heights
            .iter()
            .map(|height| {
                let block = held.best_at(*height);
                let link = block.map(|block| BlockLink { header: encode_header(block.header()) });
                link.ok_or(GetAtHeightError::HeightNotFound(*height))
            })
            .collect())
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<crate::RawMempoolTransactions, NonDomainError> {
        self.injected_cause()?;
        let held = self.held.read().expect("mock chain lock");
        Ok(listed
            .iter()
            .map(|entry| {
                let raw = held.mempool.get(&entry.txid).cloned();
                raw.ok_or(GetRawMempoolTransactionError::NotFound(entry.txid))
            })
            .collect())
    }
}
