//! One simulated zebrad ([`MockValidator`]) over a [`MockChain`]'s blocks, answering
//! [`ChainDataSource`] as zebrad does, plus what a test scripts (latency, failures, lies, verdicts)
//!
//! - best chain only, by height and hash; nothing above its tip; upgrades from the chain's schedule
//! - N nodes over one `MockChain` = N validators (`chain.validator(tip)`), each `follow`ing its tip
//! - own mempool = entries of the chain's it `hear`s + sends, each admitted on its own tip

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use zaino_primitives::testing::{encode_header, header_hash, Ledger, MockChain, Refused, Unmined};
use zaino_primitives::types::{
    Block, BlockHash, BlockRef, BlockchainInfo, DecodedHeader, EndOfService, Height, NodeRelease,
    OutPoint, PeerInfo, Script, Transaction, TransactionId, TransactionLocation,
};

use crate::{
    BlockLink, BlockLinks, ChainDataSource, CheckAdmissionError, FailureMode, GetAtHeightError,
    GetBlockByHashError, GetMempoolListingError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, MetadataReading, NonDomainError, PollReading, QueryError,
    RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};

/// `waited` within 60 s of tokio time, else the test fails naming `what`
///
/// - paused clock: 60 s virtual = instant, so a wait nothing will end (a broken driver, a fold
///   never publishing) fails at once instead of hanging the runner
pub async fn within<T>(what: &str, waited: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(60), waited).await {
        Ok(done) => done,
        Err(_) => panic!("never {what} (60 s)"),
    }
}

/// How `get_block_by_hash` / `get_block_by_height` misanswer (each caught by a body check):
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

/// One [`ChainDataSource`] method: the scope of `reachable` / `latency`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Port {
    Poll,
    Links,
    Block,
    MempoolBytes,
    Transaction,
    Send,
    Check,
}

impl Port {
    pub const ALL: [Port; 7] = [
        Port::Poll,
        Port::Links,
        Port::Block,
        Port::MempoolBytes,
        Port::Transaction,
        Port::Send,
        Port::Check,
    ];
}

/// Calls answered or refused, by port (`links` = heights asked)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Calls {
    pub polls: usize,
    pub links: usize,
    pub blocks: usize,
    pub sends: usize,
    pub checks: usize,
}

/// [`MockValidator`]s on a [`MockChain`]
pub trait ValidatorViews {
    /// Best chain = genesis ..= `tip`; reachable, accepting, mempool empty, no latency
    fn validator(&self, tip: BlockRef) -> MockValidator;
}

impl ValidatorViews for MockChain {
    fn validator(&self, tip: BlockRef) -> MockValidator {
        MockValidator::following(self, tip)
    }
}

pub struct MockValidator {
    state: Mutex<State>,
}

struct State {
    followed: Followed,
    reorg_after_poll: Option<Followed>,
    estimate: Option<Height>,
    mempool: Vec<Unmined>,
    listing: Result<(), GetMempoolListingError>,
    relay: Result<(), SendRawTransactionError>,
    checks: bool,
    peers: Option<Vec<PeerInfo>>,
    release: Option<NodeRelease>,
    latency: BTreeMap<Port, Duration>,
    failures: u32,
    failure_mode: FailureMode,
    unreachable: BTreeSet<Port>,
    lie: Option<Lie>,
    calls: Calls,
}

/// One tip of a `MockChain` as a validator holds it (`bytes` = its `raw_tx`s)
struct Followed {
    best: Vec<Arc<Block>>,
    info: BlockchainInfo,
    bytes: HashMap<TransactionId, Vec<u8>>,
    ledger: Ledger,
}

impl Followed {
    fn of(chain: &MockChain, tip: BlockRef) -> Self {
        let best = chain.blocks(tip);
        let txids = best.iter().flat_map(|block| block.transactions()).map(|tx| tx.txid);
        let bytes = txids.filter_map(|txid| Some((txid, chain.tx_bytes(txid)?.to_vec()))).collect();
        Self { best, info: chain.blockchain_info(tip), bytes, ledger: chain.ledger(tip) }
    }
}

impl MockValidator {
    fn following(chain: &MockChain, tip: BlockRef) -> Self {
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
            mempool: Vec::new(),
            listing: Ok(()),
            relay: Ok(()),
            checks: true,
            peers: Some(Vec::new()),
            release: Some(release),
            latency: BTreeMap::new(),
            failures: 0,
            failure_mode: FailureMode::Connection,
            unreachable: BTreeSet::new(),
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

    /// `chain`'s mempool entry `txid` relayed here: listed from the next poll, at the fee it
    /// leaves on this node's tip (panic: not in `chain`'s mempool, or refused on this tip)
    pub fn hear(&self, chain: &MockChain, txid: TransactionId) {
        let held = chain.mempool().iter().find(|unmined| unmined.tx.txid == txid);
        let held = held.unwrap_or_else(|| panic!("hear {txid}: not in the chain's mempool"));
        let mut state = self.state();
        let fee = state.pool().admit(&held.tx);
        let fee = fee.unwrap_or_else(|refused| panic!("hear {txid}: {refused} on its tip"));
        state.mempool.push(Unmined { fee, ..held.clone() });
    }

    /// Evicted (expiry, churn), with every entry spending its outputs: unlisted from the next poll
    pub fn evict(&self, txid: TransactionId) {
        let mut state = self.state();
        state.mempool.retain(|unmined| unmined.tx.txid != txid);
        state.readmit();
    }

    /// Followed header at `height` edited, hash recomputed (a header the builder refuses to mine)
    ///
    /// - blocks above keep their `prev_hash`: served as is, unlinked
    pub fn tamper(&self, height: Height, edit: impl FnOnce(&mut DecodedHeader)) {
        let mut state = self.state();
        let at = state.followed.best.get_mut(u32::from(height) as usize);
        let block = at.unwrap_or_else(|| panic!("tamper at {height}: above the followed tip"));
        let mut header = block.header().clone();
        edit(&mut header);
        header.hash = header_hash(&header);
        *block = Arc::new(Block::new(header, block.transactions().to_vec()));
    }

    /// `getrawmempool` answer (default `Ok`: its mempool)
    pub fn listing(&self, answer: Result<(), GetMempoolListingError>) {
        self.state().listing = answer;
    }

    /// `sendrawtransaction` + `checkadmission` verdict (default `Ok`: the fee its bytes leave over
    /// the best; only a send lists it)
    pub fn relay(&self, verdict: Result<(), SendRawTransactionError>) {
        self.state().relay = verdict;
    }

    /// `checkadmission` answered (default) or `Unsupported` (zcashd, stock zebrad)
    pub fn checks_admission(&self, supported: bool) {
        self.state().checks = supported;
    }

    /// `getpeerinfo` / `getinfo` answers (`None` = that read times out)
    pub fn metadata(&self, peers: Option<Vec<PeerInfo>>, release: Option<NodeRelease>) {
        let mut state = self.state();
        (state.peers, state.release) = (peers, release);
    }

    /// Before every answer on `ports` (`tokio::time`: a paused clock skips it)
    pub fn latency(&self, ports: &[Port], per_call: Duration) {
        let mut state = self.state();
        for port in ports {
            state.latency.insert(*port, per_call);
        }
    }

    /// Next `count` calls refused with `mode` (any port)
    pub fn fail_next(&self, count: u32, mode: FailureMode) {
        let mut state = self.state();
        (state.failures, state.failure_mode) = (count, mode);
    }

    /// `false` = every call on `ports` refused in transport until set back
    pub fn reachable(&self, ports: &[Port], reachable: bool) {
        let mut state = self.state();
        for port in ports {
            match reachable {
                true => state.unreachable.remove(port),
                false => state.unreachable.insert(*port),
            };
        }
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
    async fn enter(
        &self,
        port: Port,
        count: impl FnOnce(&mut Calls),
    ) -> Result<(), NonDomainError> {
        let latency = {
            let mut state = self.state();
            count(&mut state.calls);
            state.latency.get(&port).copied()
        };
        if let Some(latency) = latency.filter(|latency| !latency.is_zero()) {
            tokio::time::sleep(latency).await;
        }
        self.state().injected(port)
    }
}

impl State {
    /// Mined, conflicting and orphaned entries leave the mempool
    fn adopt(&mut self, followed: Followed) {
        self.followed = followed;
        self.readmit();
    }

    /// Each entry re-admitted in order on its tip, refused ones dropped
    fn readmit(&mut self) {
        let held = std::mem::take(&mut self.mempool);
        let candidates = held.into_iter().map(|unmined| (unmined.tx, unmined.bytes));
        self.mempool = self.followed.ledger.clone().readmit(candidates);
    }

    /// Its tip's ledger + every entry: what a new transaction may spend
    fn pool(&self) -> Ledger {
        let mut pool = self.followed.ledger.clone();
        for unmined in &self.mempool {
            pool.admit(&unmined.tx).expect("entries admitted in order (readmit)");
        }
        pool
    }

    fn injected(&mut self, port: Port) -> Result<(), NonDomainError> {
        if self.unreachable.contains(&port) {
            let unreachable = format!("MockValidator unreachable on {port:?}");
            return Err(NonDomainError::new(FailureMode::Connection, unreachable));
        }
        if self.failures == 0 {
            return Ok(());
        }
        self.failures -= 1;
        let mode = self.failure_mode;
        Err(NonDomainError::new(mode, format!("MockValidator injected {mode:?}")))
    }

    fn held(&self, txid: TransactionId) -> Option<&Unmined> {
        self.mempool.iter().find(|unmined| unmined.tx.txid == txid)
    }

    fn best_at(&self, height: Height) -> Option<&Arc<Block>> {
        self.followed.best.get(u32::from(height) as usize)
    }

    /// `block` as served: honest, or the lie it is told to tell
    fn answer(&self, block: &Block) -> Block {
        match self.lie {
            Some(lie) => lie.told(block),
            None => Block::clone(block),
        }
    }

    /// zebrad's verdict on `raw` over its tip + mempool: the entry it would hold, or refused
    /// (`Rejected` -25 = consensus-invalid there)
    fn verdict(&self, raw: Vec<u8>) -> Result<Unmined, SendRawTransactionError> {
        self.relay.clone()?;
        let malformed =
            |error: crate::DecodeError| SendRawTransactionError::Malformed(error.to_string());
        crate::prepare_transaction(&raw).map_err(malformed)?;
        let tx = crate::decode_transaction(&raw).map_err(malformed)?;
        let invalid = |refused: Refused| SendRawTransactionError::Rejected {
            code: -25,
            message: refused.to_string(),
        };
        let fee = self.pool().admit(&tx).map_err(invalid)?;
        Ok(Unmined { tx, bytes: raw, fee })
    }
}

impl Lie {
    /// This lie told about `honest` (pure: sans-IO models share the shapes)
    pub fn told(self, honest: &Block) -> Block {
        let (header, txs) = (honest.header().clone(), honest.transactions());
        match self {
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
}

/// Its best chain and mempool, as zebrad answers each port
impl ChainDataSource for MockValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.enter(Port::Block, |calls| calls.blocks += 1).await?;
        let state = self.state();
        let block = state.followed.best.iter().find(|block| block.header().hash == hash);
        let block = block.ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))?;
        Ok(state.answer(block))
    }

    async fn get_blocks_by_height(
        &self,
        heights: &[Height],
    ) -> Result<Vec<Block>, QueryError<GetAtHeightError>> {
        self.enter(Port::Block, |calls| calls.blocks += heights.len()).await?;
        let state = self.state();
        let answer = |height: &Height| {
            let block = state.best_at(*height);
            let absent = || QueryError::Domain(GetAtHeightError::HeightNotFound(*height));
            Ok(state.answer(block.ok_or_else(absent)?))
        };
        heights.iter().map(answer).collect()
    }

    async fn get_block_links(&self, heights: &[Height]) -> Result<BlockLinks, NonDomainError> {
        self.enter(Port::Links, |calls| calls.links += heights.len()).await?;
        let state = self.state();
        let link = |height: &Height| {
            let block = state.best_at(*height).ok_or(GetAtHeightError::HeightNotFound(*height))?;
            Ok(BlockLink { header: encode_header(block.header()) })
        };
        Ok(heights.iter().map(link).collect())
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.enter(Port::Poll, |calls| calls.polls += 1).await?;
        let mut state = self.state();
        let mut info = state.followed.info.clone();
        info.estimated_height = state.estimate.unwrap_or(info.blocks);
        if let Some(reorged) = state.reorg_after_poll.take() {
            state.adopt(reorged);
        }
        let held = holds.iter().map(|height| match state.best_at(*height) {
            Some(block) => Ok(block.header().hash),
            None => Err(QueryError::Domain(GetAtHeightError::HeightNotFound(*height))),
        });
        let listing = state.listing.clone().map(|()| {
            let listed = state.mempool.iter().map(|unmined| MempoolListed {
                txid: unmined.tx.txid,
                fee: unmined.fee,
                encoded_len: u32::try_from(unmined.bytes.len()).expect("a test tx under 4 GiB"),
            });
            let mut listed: Vec<MempoolListed> = listed.collect();
            listed.sort_by_key(|entry| entry.txid);
            listed
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
        self.enter(Port::MempoolBytes, |_| {}).await?;
        let state = self.state();
        let bytes = |entry: &MempoolListed| match state.held(entry.txid) {
            Some(unmined) => Ok(unmined.bytes.clone()),
            None => Err(GetRawMempoolTransactionError::NotFound(entry.txid)),
        };
        Ok(listed.iter().map(bytes).collect())
    }

    /// Mined without bytes (`TxBuilder`) = panic: never an invented body
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.enter(Port::Transaction, |_| {}).await?;
        let state = self.state();
        if let Some(unmined) = state.held(txid) {
            let location = TransactionLocation::Mempool;
            return Ok(TransactionResponse { bytes: unmined.bytes.clone(), location });
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
        self.enter(Port::Send, |calls| calls.sends += 1).await?;
        let mut state = self.state();
        let unmined = state.verdict(transaction).map_err(QueryError::Domain)?;
        let txid = unmined.tx.txid;
        state.mempool.push(unmined);
        Ok(txid)
    }

    async fn check_admission(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<CheckAdmissionError>> {
        self.enter(Port::Check, |calls| calls.checks += 1).await?;
        let state = self.state();
        if !state.checks {
            return Err(QueryError::Domain(CheckAdmissionError::Unsupported));
        }
        let refused = |refused| QueryError::Domain(CheckAdmissionError::Refused(refused));
        Ok(state.verdict(transaction).map_err(refused)?.tx.txid)
    }
}

/// Real, empty v4 transaction (no inputs, outputs or bundles), distinct per `lock_time`
pub fn raw_transaction(lock_time: u32, expiry: u32) -> (TransactionId, Vec<u8>) {
    v4(lock_time, expiry, None)
}

/// Real v4 transaction spending `spends`, paying `pays` in vout order; no shielded bundles
///
/// - empty script sigs (nothing here verifies a signature); distinct per `spends`
pub fn raw_transparent(spends: &[OutPoint], pays: &[(&Script, u64)]) -> (TransactionId, Vec<u8>) {
    use zcash_transparent::address::Script as RawScript;
    use zcash_transparent::bundle::{Authorized, Bundle, OutPoint as RawOutPoint, TxIn, TxOut};
    let vin = spends.iter().map(|prevout| {
        let from = RawOutPoint::new(<[u8; 32]>::from(prevout.txid), prevout.vout);
        TxIn::from_parts(from, RawScript::default(), u32::MAX)
    });
    let vout = pays.iter().map(|(script, zats)| {
        let mut prefixed = Vec::new();
        zcash_encoding::CompactSize::write(&mut prefixed, script.as_bytes().len()).expect("Vec");
        prefixed.extend_from_slice(script.as_bytes());
        let script = RawScript::read(&prefixed[..]).expect("a length-prefixed script");
        let value = zcash_protocol::value::Zatoshis::from_u64(*zats).expect("within the supply");
        TxOut::new(value, script)
    });
    let bundle = Bundle { vin: vin.collect(), vout: vout.collect(), authorization: Authorized };
    v4(0, 0, Some(bundle))
}

/// Real, empty v5 transaction signed for `branch` (embedded in its bytes), distinct per
/// `lock_time`
pub fn raw_v5(
    branch: zcash_protocol::consensus::BranchId,
    lock_time: u32,
    expiry: u32,
) -> (TransactionId, Vec<u8>) {
    use zcash_primitives::transaction::TxVersion;
    unshielded(TxVersion::V5, branch, lock_time, expiry, None)
}

fn v4(
    lock_time: u32,
    expiry: u32,
    transparent: Option<zcash_transparent::bundle::Bundle<zcash_transparent::bundle::Authorized>>,
) -> (TransactionId, Vec<u8>) {
    use zcash_primitives::transaction::TxVersion;
    let canopy = zcash_protocol::consensus::BranchId::Canopy;
    unshielded(TxVersion::V4, canopy, lock_time, expiry, transparent)
}

fn unshielded(
    version: zcash_primitives::transaction::TxVersion,
    branch: zcash_protocol::consensus::BranchId,
    lock_time: u32,
    expiry: u32,
    transparent: Option<zcash_transparent::bundle::Bundle<zcash_transparent::bundle::Authorized>>,
) -> (TransactionId, Vec<u8>) {
    use zcash_primitives::transaction::{Authorized, TransactionData};
    let tx = TransactionData::<Authorized>::from_parts(
        version,
        branch,
        lock_time,
        expiry.into(),
        transparent,
        None,
        None,
        None,
    )
    .freeze()
    .expect("a transaction without shielded bundles freezes");
    let mut raw = Vec::new();
    tx.write(&mut raw).expect("writes to a Vec");
    (TransactionId::from(*tx.txid().as_ref()), raw)
}

/// `raw` decoded beside its bytes (`BlockBuilder::raw_tx(decoded(raw))`)
pub fn decoded(raw: Vec<u8>) -> (Transaction, Vec<u8>) {
    let tx = crate::decode_transaction(&raw).expect("decoded takes real transaction bytes");
    (tx, raw)
}

pub mod fixtures;

#[cfg(test)]
mod tests;
