//! [`MockValidator`]: one simulated zebrad over a [`MockChain`]'s blocks, answering
//! [`ChainDataSource`] as zebrad does, plus what a test scripts (latency, failures, lies, verdicts)
//!
//! - best chain only, by height and hash; nothing above its tip; upgrades from the chain's schedule
//! - N nodes over one `MockChain` = N validators, each `follow`ing its own tip

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use zaino_primitives::testing::{encode_header, fee_left, header_hash, MockChain};
use zaino_primitives::types::{
    Block, BlockHash, BlockRef, BlockchainInfo, EndOfService, Height, NodeRelease, OutPoint,
    PeerInfo, Transaction, TransactionId, TransactionLocation, Zatoshis,
};

use crate::{
    BlockLink, BlockLinks, ChainDataSource, FailureMode, GetBlockByHashError, GetBlockError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, NonDomainError, PollReading, QueryError, RawMempoolTransactions,
    SendRawTransactionError, TransactionResponse,
};

/// How `get_block_by_hash` misanswers (each caught by the NFS body check):
/// - `WrongBlock` = the asked block re-mined under another nonce
/// - `Poisoned` = asked header + one extra transaction; `Mutated` = last tx repeated (CVE-2012-2459)
/// - `WrongHeight` = the asked block labelled one height up
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lie {
    WrongBlock,
    Poisoned,
    Mutated,
    WrongHeight,
}

/// Calls answered or refused, by port (`links` = heights asked)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Calls {
    pub polls: usize,
    pub links: usize,
    pub blocks: usize,
    pub sends: usize,
}

pub struct MockValidator {
    state: Mutex<State>,
}

struct State {
    followed: Followed,
    reorg_after_poll: Option<Followed>,
    estimate: Option<Height>,
    mempool: BTreeMap<TransactionId, (Vec<u8>, Zatoshis)>,
    listing: Result<(), GetMempoolListingError>,
    relay: Result<(), SendRawTransactionError>,
    peers: Option<Vec<PeerInfo>>,
    release: Option<NodeRelease>,
    latency: Duration,
    failures: u32,
    failure_mode: FailureMode,
    reachable: bool,
    lie: Option<Lie>,
    calls: Calls,
}

/// One tip of a `MockChain` as a validator holds it (`bytes` = its `raw_tx`s)
struct Followed {
    best: Vec<Arc<Block>>,
    info: BlockchainInfo,
    bytes: HashMap<TransactionId, Vec<u8>>,
}

impl Followed {
    fn of(chain: &MockChain, tip: BlockRef) -> Self {
        let best = chain.blocks(tip);
        let txids = best.iter().flat_map(|block| block.transactions()).map(|tx| tx.txid);
        let bytes = txids.filter_map(|txid| Some((txid, chain.tx_bytes(txid)?.to_vec()))).collect();
        Self { best, info: chain.blockchain_info(tip), bytes }
    }
}

impl MockValidator {
    /// Best chain = genesis ..= `tip`; reachable, accepting, mempool empty, no latency
    pub fn following(chain: &MockChain, tip: BlockRef) -> Self {
        let release = NodeRelease {
            build: "mock".to_owned(),
            user_agent: "/MockValidator/".to_owned(),
            protocol_version: 0,
            end_of_service: EndOfService::NotEnforced,
        };
        let state = State {
            followed: Followed::of(chain, tip),
            reorg_after_poll: None,
            estimate: None,
            mempool: BTreeMap::new(),
            listing: Ok(()),
            relay: Ok(()),
            peers: Some(Vec::new()),
            release: Some(release),
            latency: Duration::ZERO,
            failures: 0,
            failure_mode: FailureMode::Connection,
            reachable: true,
            lie: None,
            calls: Calls::default(),
        };
        Self { state: Mutex::new(state) }
    }

    /// Best chain = genesis ..= `tip` (extend, reorg, retreat: any held tip)
    pub fn follow(&self, chain: &MockChain, tip: BlockRef) {
        self.state().adopt(Followed::of(chain, tip));
    }

    /// Next poll reads its tip, then reorgs to `tip` before its `getblockhash` answers
    pub fn reorg_after_next_poll(&self, chain: &MockChain, tip: BlockRef) {
        self.state().reorg_after_poll = Some(Followed::of(chain, tip));
    }

    /// `estimatedheight` from now on (default: its tip)
    pub fn estimate(&self, height: Height) {
        self.state().estimate = Some(height);
    }

    /// Listed from the next poll at `fee` zats
    pub fn mempool_insert(&self, raw: Vec<u8>, fee: u64) -> TransactionId {
        let txid = crate::prepare_transaction(&raw).expect("mempool_insert takes real bytes").txid;
        let fee = Zatoshis::new(fee).expect("a fee within the supply");
        self.state().mempool.insert(txid, (raw, fee));
        txid
    }

    /// `getrawmempool` answer (default `Ok`: its mempool)
    pub fn listing(&self, answer: Result<(), GetMempoolListingError>) {
        self.state().listing = answer;
    }

    /// `sendrawtransaction` verdict (default `Ok`: listed at the fee its bytes leave over the best)
    pub fn relay(&self, verdict: Result<(), SendRawTransactionError>) {
        self.state().relay = verdict;
    }

    /// `getpeerinfo` / `getinfo` answers (`None` = that read times out)
    pub fn metadata(&self, peers: Option<Vec<PeerInfo>>, release: Option<NodeRelease>) {
        let mut state = self.state();
        (state.peers, state.release) = (peers, release);
    }

    /// Before every answer (`tokio::time`: a paused clock skips it)
    pub fn latency(&self, per_call: Duration) {
        self.state().latency = per_call;
    }

    /// Next `count` calls refused with `mode` (any port)
    pub fn fail_next(&self, count: u32, mode: FailureMode) {
        let mut state = self.state();
        (state.failures, state.failure_mode) = (count, mode);
    }

    /// `false` = every call refused in transport until set back
    pub fn reachable(&self, reachable: bool) {
        self.state().reachable = reachable;
    }

    pub fn lie(&self, lie: Option<Lie>) {
        self.state().lie = lie;
    }

    pub fn calls(&self) -> Calls {
        self.state().calls
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("MockValidator mutex poisoned")
    }

    /// Counted, delayed, then refused if injected
    async fn enter(&self, count: impl FnOnce(&mut Calls)) -> Result<(), NonDomainError> {
        let latency = {
            let mut state = self.state();
            count(&mut state.calls);
            state.latency
        };
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        self.state().injected()
    }
}

impl State {
    /// Mined txids leave the mempool
    fn adopt(&mut self, followed: Followed) {
        for tx in followed.best.iter().flat_map(|block| block.transactions()) {
            self.mempool.remove(&tx.txid);
        }
        self.followed = followed;
    }

    fn injected(&mut self) -> Result<(), NonDomainError> {
        if !self.reachable {
            return Err(NonDomainError::new(FailureMode::Connection, "MockValidator unreachable"));
        }
        if self.failures == 0 {
            return Ok(());
        }
        self.failures -= 1;
        let mode = self.failure_mode.clone();
        Err(NonDomainError::new(mode.clone(), format!("MockValidator injected {mode:?}")))
    }

    fn best_at(&self, height: Height) -> Option<&Arc<Block>> {
        self.followed.best.get(u32::from(height) as usize)
    }

    /// zebrad's verdict on `raw` over its best chain: listed at its fee, or refused
    fn admit(&mut self, raw: Vec<u8>) -> Result<TransactionId, SendRawTransactionError> {
        let malformed =
            |error: crate::DecodeError| SendRawTransactionError::Malformed(error.to_string());
        let txid = crate::prepare_transaction(&raw).map_err(malformed)?.txid;
        let tx = crate::decode_transaction(&raw).map_err(malformed)?;
        let fee = fee_over(&self.followed.best, &tx)?;
        self.mempool.insert(txid, (raw, fee));
        Ok(txid)
    }
}

/// Inputs from the best chain's unspent outputs; `Rejected` = an unknown input or an overspend
fn fee_over(best: &[Arc<Block>], tx: &Transaction) -> Result<Zatoshis, SendRawTransactionError> {
    let mut unspent: HashMap<OutPoint, Zatoshis> = HashMap::new();
    for mined in best.iter().flat_map(|block| block.transactions()) {
        for prevout in &mined.transparent.inputs {
            unspent.remove(prevout);
        }
        for (vout, output) in (0..).zip(&mined.transparent.outputs) {
            unspent.insert(OutPoint { txid: mined.txid, vout }, output.value);
        }
    }
    let mut spent = 0;
    for prevout in &tx.transparent.inputs {
        let value = unspent.get(prevout).ok_or_else(|| {
            let missing = format!("missing input {}:{}", prevout.txid, prevout.vout);
            SendRawTransactionError::Rejected(missing)
        })?;
        spent += value.as_i64();
    }
    let fee = fee_left(tx, spent);
    let fee = u64::try_from(fee).ok().and_then(|fee| Zatoshis::new(fee).ok());
    fee.ok_or_else(|| SendRawTransactionError::Rejected(format!("{} overspends", tx.txid)))
}

/// `lie` applied to the honest `block`
fn misanswer(block: &Block, lie: Lie) -> Block {
    let (header, txs) = (block.header().clone(), block.transactions());
    match lie {
        Lie::WrongBlock => {
            let mut other = header;
            other.nonce[31] ^= 0xff;
            other.hash = header_hash(&other);
            Block::new(other, txs.to_vec())
        }
        Lie::Poisoned => {
            let mut extra = txs[0].clone();
            let mut txid = <[u8; 32]>::from(extra.txid);
            txid[31] ^= 0xff;
            extra.txid = TransactionId::from(txid);
            Block::new(header, [txs, &[extra]].concat())
        }
        Lie::Mutated => Block::new(header, [txs, &txs[txs.len() - 1..]].concat()),
        Lie::WrongHeight => {
            let mut labelled = header;
            labelled.height = labelled.height.next();
            Block::new(labelled, txs.to_vec())
        }
    }
}

/// Its best chain and mempool, as zebrad answers each port
impl ChainDataSource for MockValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.enter(|calls| calls.blocks += 1).await?;
        let state = self.state();
        let block = state.followed.best.iter().find(|block| block.header().hash == hash);
        let block = block.ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))?;
        Ok(match state.lie {
            Some(lie) => misanswer(block, lie),
            None => Block::clone(block),
        })
    }

    async fn get_block_links(&self, heights: &[Height]) -> Result<BlockLinks, NonDomainError> {
        self.enter(|calls| calls.links += heights.len()).await?;
        let state = self.state();
        let link = |height: &Height| {
            let block = state.best_at(*height).ok_or(GetBlockError::HeightNotFound(*height))?;
            Ok(BlockLink { header: encode_header(block.header()) })
        };
        Ok(heights.iter().map(link).collect())
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.enter(|calls| calls.polls += 1).await?;
        let mut state = self.state();
        let mut info = state.followed.info.clone();
        info.estimated_height = state.estimate.unwrap_or(info.blocks);
        if let Some(reorged) = state.reorg_after_poll.take() {
            state.adopt(reorged);
        }
        let held = holds.iter().map(|height| match state.best_at(*height) {
            Some(block) => Ok(block.header().hash),
            None => Err(QueryError::Domain(GetBlockError::HeightNotFound(*height))),
        });
        let listing = state.listing.clone().map(|()| {
            let listed = state.mempool.iter().map(|(txid, (raw, fee))| MempoolListed {
                txid: *txid,
                fee: *fee,
                encoded_len: u32::try_from(raw.len()).expect("a test transaction under 4 GiB"),
            });
            listed.collect()
        });
        let timed_out = || NonDomainError::new(FailureMode::Timeout, "MockValidator metadata");
        let metadata = metadata.then(|| MetadataReading {
            peers: state.peers.clone().ok_or_else(timed_out),
            release: state.release.clone().ok_or_else(timed_out),
        });
        Ok(PollReading { info, listing, held: held.collect(), metadata })
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        self.enter(|_| {}).await?;
        let state = self.state();
        let bytes = |entry: &MempoolListed| match state.mempool.get(&entry.txid) {
            Some((raw, _)) => Ok(raw.clone()),
            None => Err(GetRawMempoolTransactionError::NotFound(entry.txid)),
        };
        Ok(listed.iter().map(bytes).collect())
    }

    /// Mined without bytes (`TxBuilder`) = panic: never an invented body
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.enter(|_| {}).await?;
        let state = self.state();
        if let Some((raw, _)) = state.mempool.get(&txid) {
            let location = TransactionLocation::Mempool;
            return Ok(TransactionResponse { bytes: raw.clone(), location });
        }
        let mined = state
            .followed
            .best
            .iter()
            .find(|block| block.transactions().iter().any(|tx| tx.txid == txid));
        let block = mined.ok_or(QueryError::Domain(GetTransactionError::NotFound(txid)))?;
        let location = TransactionLocation::BestChain(block.header().height);
        let bytes = state.followed.bytes.get(&txid).cloned();
        drop(state); // unlocked before the panic (mutex never poisoned)
        let bytes = bytes.unwrap_or_else(|| {
            panic!("{txid} mined without bytes: mine it with BlockBuilder::raw_tx")
        });
        Ok(TransactionResponse { bytes, location })
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        self.enter(|calls| calls.sends += 1).await?;
        let mut state = self.state();
        state.relay.clone().map_err(QueryError::Domain)?;
        state.admit(transaction).map_err(QueryError::Domain)
    }
}

/// Real, empty v4 transaction (no inputs, outputs or bundles), distinct per `lock_time`
pub fn raw_transaction(lock_time: u32, expiry: u32) -> (TransactionId, Vec<u8>) {
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V4,
        zcash_protocol::consensus::BranchId::Canopy,
        lock_time,
        expiry.into(),
        None,
        None,
        None,
        None,
    )
    .freeze()
    .expect("an empty v4 transaction freezes");
    let mut raw = Vec::new();
    tx.write(&mut raw).expect("writes to a Vec");
    (TransactionId::from(*tx.txid().as_ref()), raw)
}

/// `raw` decoded beside its bytes (`BlockBuilder::raw_tx(decoded(raw))`)
pub fn decoded(raw: Vec<u8>) -> (Transaction, Vec<u8>) {
    let tx = crate::decode_transaction(&raw).expect("decoded takes real transaction bytes");
    (tx, raw)
}

#[cfg(test)]
mod tests;
