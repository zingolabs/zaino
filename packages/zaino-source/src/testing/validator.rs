//! [`MockValidator`]: one simulated zebrad over a [`MockChain`]'s blocks, answering zebrad's
//! JSON-RPC as bytes
//!
//! - tests reach it through Zaino's own client ([`MockValidator::client`])
//! - best chain only, by height and hash (nothing above its tip)
//! - upgrades from the chain's schedule
//! - N nodes over one `MockChain` = N validators (`chain.validator(tip)`), each `follow`ing its tip
//! - own mempool = entries of the chain's it `hear`s + sends, each admitted on its own tip
//! - scripted by wire method name (`latency`, `reachable`, `lacks`, `calls`), checked against
//!   [`METHODS`]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::FutureExt;
use serde_json::{json, Value};
use zaino_primitives::types::{Block, BlockRef, DecodedHeader, HeaderBytes};
use zcash_encoding::CompactSize;
use zcash_primitives::transaction::{CompressedTransaction, CompressedTransactionData, TxVersion};
use zcash_protocol::consensus::{BlockHeight, BranchId, Parameters, UPGRADES_IN_ORDER};
use zcash_protocol::{BlockHash, TxId};
use zcash_script::script::Code;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::{Bundle as TransparentBundle, TxIn};

use super::{
    encode_header, header_hash, mint, raw_transaction, Ledger, MockChain, Refused, Unmined,
};
use crate::rpc::{LinkLimits, RpcClient, RpcError, Served};
use crate::{
    BlockchainInfo, EndOfService, GetMempoolListingError, NodeRelease, PeerInfo,
    SendRawTransactionError, ZcashLegacyRpcClient,
};

/// zebrad's JSON-RPC methods this node answers
///
/// - any other name = `-32601` (what an older node answers)
const METHODS: &[(&str, Handler)] = &[
    ("getblockchaininfo", getblockchaininfo),
    ("getrawmempool", getrawmempool),
    ("getblockhash", getblockhash),
    ("getblockheader", getblockheader),
    ("getblock", getblock),
    ("getrawtransaction", getrawtransaction),
    ("sendrawtransaction", sendrawtransaction),
    ("checkadmission", checkadmission),
    ("getstandardfee", getstandardfee),
    ("getpeerinfo", getpeerinfo),
    ("getinfo", getinfo),
    ("getdeprecationinfo", getdeprecationinfo),
];

/// One method: `params` in, `result` or zebrad's `(code, message)` out
type Handler = fn(&mut Node, &[Value]) -> Reply;
type Reply = Result<Value, (i64, String)>;

fn method_not_found() -> (i64, String) {
    (-32601, "Method not found".to_owned())
}

// ---- the methods, as zebrad answers them ----

/// Tip, estimate, upgrade schedule
///
/// - a scripted reorg lands right after (the rest of this poll's batch reads the new chain)
fn getblockchaininfo(node: &mut Node, _: &[Value]) -> Reply {
    let info = &node.chain.info;
    let upgrades = info.activations.iter().map(|(branch, height)| {
        let upgrade =
            json!({ "name": format!("{branch:?}"), "activationheight": u32::from(*height) });
        (format!("{:08x}", u32::from(*branch)), upgrade)
    });
    let answer = json!({
        "chain": "regtest",
        "blocks": u32::from(info.tip.height),
        "bestblockhash": info.tip.hash.to_string(),
        "estimatedheight": u32::from(node.estimate.unwrap_or(info.tip.height)),
        "upgrades": Value::Object(upgrades.collect()),
    });
    if let Some(reorged) = node.reorg_after_poll.take() {
        node.adopt(reorged);
    }
    Ok(answer)
}

/// `getrawmempool true`: `{txid: {size, fee, ..}}`, fee in ZEC
fn getrawmempool(node: &mut Node, _: &[Value]) -> Reply {
    match node.listing {
        Ok(()) => {}
        Err(GetMempoolListingError::Inactive) => {
            return Err((-1, "mempool is not active: wait for Zebra to sync to the tip".into()));
        }
        Err(GetMempoolListingError::Unavailable) => return Err(method_not_found()),
    }
    let entries = node.mempool.iter().map(|unmined| {
        let fee = unmined.fee.into_u64() as f64 / 100_000_000.0;
        let entry =
            json!({ "size": unmined.bytes.len(), "fee": fee, "modifiedfee": fee, "depends": [] });
        (unmined.tx.txid.to_string(), entry)
    });
    Ok(Value::Object(entries.collect()))
}

/// `getblockhash <height>`: above the tip = zebrad's `-32602`
fn getblockhash(node: &mut Node, params: &[Value]) -> Reply {
    let height = params.first().and_then(Value::as_u64).ok_or_else(|| invalid("a height"))?;
    match node.chain.at(height) {
        Some(held) => Ok(json!(held.hash().to_string())),
        None => Err((-32602, "Provided index is greater than the current tip".into())),
    }
}

/// `getblockheader <height | hash> false`: the header's hex
fn getblockheader(node: &mut Node, params: &[Value]) -> Reply {
    let held = node.chain.find(params.first())?;
    Ok(json!(const_hex::encode(encode_header(held.block.header()))))
}

/// `getblock <height | hash> 0`: the block's hex, or the lie it is scripted to tell
fn getblock(node: &mut Node, params: &[Value]) -> Reply {
    let held = node.chain.find(params.first())?;
    let raw = match node.lie {
        Some(lie) => lie.told(&held.raw),
        None => held.raw.clone(),
    };
    Ok(json!(const_hex::encode(raw)))
}

/// `getrawtransaction <txid> 0 | 1`: its mempool first, then its best chain (`1` = `{hex, height}`)
fn getrawtransaction(node: &mut Node, params: &[Value]) -> Reply {
    let txid = params.first().and_then(Value::as_str).and_then(TxId::from_hex);
    let txid = txid.ok_or_else(|| invalid("a txid"))?;
    let verbose = params.get(1).and_then(Value::as_u64) == Some(1);
    let not_found = || (-5, "No such mempool or main chain transaction".to_owned());
    let (bytes, height) = match node.held(txid) {
        Some(unmined) => (unmined.bytes.clone(), None),
        None => {
            let held = node.chain.containing(txid).ok_or_else(not_found)?;
            (node.chain.tx_bytes[&txid].clone(), Some(held.block.header().height))
        }
    };
    let hex = const_hex::encode(bytes);
    match (verbose, height) {
        (false, _) => Ok(json!(hex)),
        (true, None) => Ok(json!({ "hex": hex })),
        (true, Some(height)) => Ok(json!({ "hex": hex, "height": u32::from(height) })),
    }
}

/// Judged on its tip + mempool (accepted = listed from the next poll)
fn sendrawtransaction(node: &mut Node, params: &[Value]) -> Reply {
    let unmined = node.verdict(hex_param(params)?).map_err(refusal)?;
    let txid = unmined.tx.txid;
    node.mempool.push(unmined);
    Ok(json!(txid.to_string()))
}

/// `sendrawtransaction`'s verdict, nothing listed (zebra fork: zcashd + stock zebrad `lack` it)
fn checkadmission(node: &mut Node, params: &[Value]) -> Reply {
    let unmined = node.verdict(hex_param(params)?).map_err(refusal)?;
    Ok(json!(unmined.tx.txid.to_string()))
}

/// ZIP 2009's marginal fee, the static estimator
fn getstandardfee(_: &mut Node, _: &[Value]) -> Reply {
    Ok(json!({ "standard_fee": 1_000, "version": 0 }))
}

fn getpeerinfo(node: &mut Node, _: &[Value]) -> Reply {
    let peers = node.peers.as_ref().ok_or_else(|| scripted_failure("getpeerinfo"))?;
    let peers = peers.iter().map(|peer| json!({ "addr": peer.addr, "inbound": peer.inbound }));
    Ok(Value::Array(peers.collect()))
}

fn getinfo(node: &mut Node, _: &[Value]) -> Reply {
    let release = node.release.as_ref().ok_or_else(|| scripted_failure("getinfo"))?;
    Ok(json!({
        "build": release.build,
        "subversion": release.user_agent,
        "blocks": u32::from(node.chain.info.tip.height),
    }))
}

/// zebrad ≥ 6.3 (`EndOfService::Unknown` = an older one: no such method)
fn getdeprecationinfo(node: &mut Node, _: &[Value]) -> Reply {
    let release = node.release.as_ref().ok_or_else(|| scripted_failure("getinfo"))?;
    match release.end_of_service {
        EndOfService::Unknown => Err(method_not_found()),
        EndOfService::NotEnforced => Ok(json!({})),
        EndOfService::At { height } => {
            Ok(json!({ "end_of_service": { "block_height": u32::from(height) } }))
        }
    }
}

fn hex_param(params: &[Value]) -> Result<Vec<u8>, (i64, String)> {
    let hex = params.first().and_then(Value::as_str).ok_or_else(|| invalid("a hex string"))?;
    const_hex::decode(hex).map_err(|_| (-22, "TX decode failed".to_owned()))
}

fn invalid(expected: &str) -> (i64, String) {
    (-32602, format!("invalid params: expected {expected}"))
}

fn scripted_failure(method: &str) -> (i64, String) {
    (-1, format!("MockValidator: {method} scripted to fail"))
}

/// A verdict as zebrad's error: `Malformed` = `-22`, `Rejected` = its own code
fn refusal(refused: SendRawTransactionError) -> (i64, String) {
    (i64::from(refused.code()), refused.message().to_owned())
}

// ---- one request: bytes in, bytes out ----

/// One HTTP body (a call or a batch): faults first (a slow or dead node, as seen from outside),
/// then each call answered in order
async fn serve(node: Arc<Mutex<Node>>, request: Vec<u8>) -> Result<Vec<u8>, RpcError> {
    let request: Value = serde_json::from_slice(&request)?;
    let (calls, batch) = match request {
        Value::Array(calls) => (calls, true),
        call => (vec![call], false),
    };
    let methods: Vec<&str> =
        calls.iter().map(|call| call["method"].as_str().unwrap_or_default()).collect();

    let delay = lock(&node).slowest(&methods);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if lock(&node).refuses(&methods) {
        return Err(RpcError::Status(503));
    }

    let mut node = lock(&node);
    let mut replies: Vec<Value> = calls.iter().map(|call| node.answer(call)).collect();
    let reply = if batch { Value::Array(replies) } else { replies.remove(0) };
    Ok(serde_json::to_vec(&reply)?)
}

impl Node {
    /// One call → its JSON-RPC reply object (counted whatever it answers)
    fn answer(&mut self, call: &Value) -> Value {
        let id = call["id"].clone();
        let method = call["method"].as_str().unwrap_or_default();
        let params = call["params"].as_array().map(Vec::as_slice).unwrap_or_default();
        *self.calls.entry(method.to_owned()).or_default() += 1;

        let lacked = self.lacks.contains(method);
        let handler = METHODS.iter().find(|(name, _)| *name == method && !lacked);
        let outcome = match handler {
            Some((_, handler)) => handler(self, params),
            None => Err(method_not_found()),
        };
        match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        }
    }

    fn slowest(&self, methods: &[&str]) -> Duration {
        let latency = methods.iter().filter_map(|method| self.latency.get(method));
        latency.copied().max().unwrap_or_default()
    }

    fn refuses(&self, methods: &[&str]) -> bool {
        self.down || methods.iter().any(|method| self.unreachable.contains(method))
    }
}

// ---- the node: what it holds and what a test scripts ----

/// [`MockValidator`]s on a [`MockChain`]
pub trait ValidatorViews {
    /// Best chain = genesis ..= `tip`, reachable, accepting, mempool empty, no latency
    fn validator(&self, tip: BlockRef) -> MockValidator;

    /// `getblockchaininfo` with `tip` as the validator's best
    fn blockchain_info(&self, tip: BlockRef) -> BlockchainInfo;
}

impl ValidatorViews for MockChain {
    fn validator(&self, tip: BlockRef) -> MockValidator {
        MockValidator::following(self, tip)
    }

    fn blockchain_info(&self, tip: BlockRef) -> BlockchainInfo {
        assert_eq!(self.block(tip.hash).block_ref(), tip, "tip held at its height");
        let upgrades = &self.schedule().upgrades;
        let scheduled = UPGRADES_IN_ORDER.iter().filter_map(|&upgrade| {
            Some((upgrade.branch_id(), upgrades.activation_height(upgrade)?))
        });
        BlockchainInfo { tip, estimated_height: tip.height, activations: scheduled.collect() }
    }
}

/// One simulated zebrad (clones = the same node)
#[derive(Clone)]
pub struct MockValidator {
    node: Arc<Mutex<Node>>,
}

struct Node {
    chain: Followed,
    reorg_after_poll: Option<Followed>,
    estimate: Option<BlockHeight>,
    mempool: Vec<Unmined>,
    listing: Result<(), GetMempoolListingError>,
    relay: Result<(), SendRawTransactionError>,
    peers: Option<Vec<PeerInfo>>,
    release: Option<NodeRelease>,
    lie: Option<Lie>,
    latency: BTreeMap<&'static str, Duration>,
    unreachable: BTreeSet<&'static str>,
    lacks: BTreeSet<&'static str>,
    down: bool,
    calls: HashMap<String, usize>,
}

/// One tip of a `MockChain` as a node holds it (`best[h]` = its block at height `h`)
struct Followed {
    best: Vec<Held>,
    info: BlockchainInfo,
    tx_bytes: HashMap<TxId, Vec<u8>>,
    ledger: Ledger,
}

/// One block, decoded beside the bytes `getblock` serves
struct Held {
    block: Arc<Block>,
    raw: Vec<u8>,
}

impl MockValidator {
    fn following(chain: &MockChain, tip: BlockRef) -> Self {
        let release = NodeRelease {
            build: "mock".to_owned(),
            user_agent: "/MockValidator/".to_owned(),
            end_of_service: EndOfService::NotEnforced,
        };
        let node = Node {
            chain: Followed::of(chain, tip),
            reorg_after_poll: None,
            estimate: None,
            mempool: Vec::new(),
            listing: Ok(()),
            relay: Ok(()),
            peers: Some(Vec::new()),
            release: Some(release),
            lie: None,
            latency: BTreeMap::new(),
            unreachable: BTreeSet::new(),
            lacks: BTreeSet::new(),
            down: false,
            calls: HashMap::new(),
        };
        Self { node: Arc::new(Mutex::new(node)) }
    }

    /// Zaino's own JSON-RPC client, its requests answered by this node in-process
    pub fn client(&self) -> ZcashLegacyRpcClient {
        let node = Arc::clone(&self.node);
        let served: Served = Arc::new(move |request| serve(Arc::clone(&node), request).boxed());
        let rpc = RpcClient::served("mock".to_owned(), served, LinkLimits::default());
        ZcashLegacyRpcClient { rpc }
    }

    /// Best chain = genesis ..= `tip` (extend, reorg, retreat: any held tip)
    pub fn follow(&self, chain: &MockChain, tip: BlockRef) {
        self.node().adopt(Followed::of(chain, tip));
    }

    /// Next poll reads its tip, then reorgs to `tip` before its `getblockhash` answers
    pub fn reorg_after_next_poll(&self, chain: &MockChain, tip: BlockRef) {
        self.node().reorg_after_poll = Some(Followed::of(chain, tip));
    }

    /// `estimatedheight` from now on (default: its tip)
    pub fn estimate(&self, height: BlockHeight) {
        self.node().estimate = Some(height);
    }

    /// `chain`'s mempool entry `txid` relayed here: listed from the next poll, at the fee it
    /// leaves on this node's tip (panic: not in `chain`'s mempool, or refused on this tip)
    pub fn hear(&self, chain: &MockChain, txid: TxId) {
        let held = chain.mempool().iter().find(|unmined| unmined.tx.txid == txid);
        let held = held.unwrap_or_else(|| panic!("hear {txid}: not in the chain's mempool"));
        let mut node = self.node();
        let fee = node.pool().admit(&held.tx);
        let fee = fee.unwrap_or_else(|refused| panic!("hear {txid}: {refused} on its tip"));
        node.mempool.push(Unmined { fee, ..held.clone() });
    }

    /// Evicted (expiry, churn), with every entry spending its outputs: unlisted from the next poll
    pub fn evict(&self, txid: TxId) {
        let mut node = self.node();
        node.mempool.retain(|unmined| unmined.tx.txid != txid);
        node.readmit();
    }

    /// Followed header at `height` edited, hash recomputed (a header the builder refuses to mine)
    ///
    /// - blocks above keep their `prev_hash`: served as is, unlinked
    pub fn tamper(&self, height: BlockHeight, edit: impl FnOnce(&mut DecodedHeader)) {
        let mut node = self.node();
        let at = node.chain.best.get_mut(u32::from(height) as usize);
        let held = at.unwrap_or_else(|| panic!("tamper at {height}: above the followed tip"));
        let mut header = held.block.header().clone();
        edit(&mut header);
        header.hash = header_hash(&header);
        let (_, body) = HeaderBytes::split(&held.raw).expect("a held block's header");
        let raw = [encode_header(&header), body.to_vec()].concat();
        *held = Held::of(raw);
    }

    /// `getrawmempool` answer (default `Ok`: its mempool)
    pub fn listing(&self, answer: Result<(), GetMempoolListingError>) {
        self.node().listing = answer;
    }

    /// `sendrawtransaction` + `checkadmission` verdict (default `Ok`: the fee its bytes leave over
    /// the best)
    ///
    /// - only a send lists it
    pub fn relay(&self, verdict: Result<(), SendRawTransactionError>) {
        self.node().relay = verdict;
    }

    /// An older node: each of `methods` answers `-32601` (zcashd and stock zebrad lack
    /// `checkadmission`)
    pub fn lacks(&self, methods: &[&str]) {
        self.node().lacks.extend(methods.iter().map(|name| method(name)));
    }

    /// `getpeerinfo` / `getinfo` answers (`None` = that call fails)
    pub fn metadata(&self, peers: Option<Vec<PeerInfo>>, release: Option<NodeRelease>) {
        let mut node = self.node();
        (node.peers, node.release) = (peers, release);
    }

    /// Before every request carrying one of `methods` (`tokio::time`: a paused clock skips it)
    pub fn latency(&self, methods: &[&str], per_request: Duration) {
        let mut node = self.node();
        for name in methods {
            node.latency.insert(method(name), per_request);
        }
    }

    /// `false` = every request carrying one of `methods` refused (HTTP 503) until set back
    pub fn reachable(&self, methods: &[&str], reachable: bool) {
        let mut node = self.node();
        for name in methods {
            match reachable {
                true => node.unreachable.remove(method(name)),
                false => node.unreachable.insert(method(name)),
            };
        }
    }

    /// `true` = every request refused (HTTP 503), whatever it carries
    pub fn down(&self, down: bool) {
        self.node().down = down;
    }

    pub fn lie(&self, lie: Option<Lie>) {
        self.node().lie = lie;
    }

    /// Calls of `method` answered or refused in an answered request (a batch counts each item)
    pub fn calls(&self, name: &str) -> usize {
        self.node().calls.get(method(name)).copied().unwrap_or(0)
    }

    fn node(&self) -> MutexGuard<'_, Node> {
        lock(&self.node)
    }
}

fn lock(node: &Mutex<Node>) -> MutexGuard<'_, Node> {
    node.lock().expect("MockValidator mutex poisoned")
}

/// `name` as the dispatch table spells it (a typo in a test panics here)
fn method(name: &str) -> &'static str {
    let found = METHODS.iter().find(|(served, _)| *served == name);
    found
        .map(|(served, _)| *served)
        .unwrap_or_else(|| panic!("{name}: not a method MockValidator serves"))
}

impl Followed {
    fn of(chain: &MockChain, tip: BlockRef) -> Self {
        let best: Vec<Held> = chain
            .blocks(tip)
            .iter()
            .map(|block| Held::of(chain.block_bytes(block.header().hash).to_vec()))
            .collect();
        let txids = best.iter().flat_map(|held| held.block.transactions()).map(|tx| tx.txid);
        let tx_bytes =
            txids.map(|txid| (txid, chain.tx_bytes(txid).expect("minted").to_vec())).collect();
        Self { best, info: chain.blockchain_info(tip), tx_bytes, ledger: chain.ledger(tip) }
    }

    fn at(&self, height: u64) -> Option<&Held> {
        self.best.get(usize::try_from(height).ok()?)
    }

    /// A `getblock` / `getblockheader` argument: a height (`-8` above the tip) or a hash (`-5`)
    fn find(&self, param: Option<&Value>) -> Result<&Held, (i64, String)> {
        let param = param.and_then(Value::as_str).ok_or_else(|| invalid("a height or a hash"))?;
        if let Ok(height) = param.parse::<u64>() {
            return self.at(height).ok_or_else(|| (-8, "Block not found".to_owned()));
        }
        let hash = BlockHash::from_hex(param).ok_or_else(|| invalid("a block hash"))?;
        let held = self.best.iter().find(|held| held.hash() == hash);
        held.ok_or_else(|| (-5, "Block not found".to_owned()))
    }

    fn containing(&self, txid: TxId) -> Option<&Held> {
        self.best.iter().find(|held| held.block.transactions().iter().any(|tx| tx.txid == txid))
    }
}

impl Held {
    fn of(raw: Vec<u8>) -> Self {
        let block = crate::decode_block(&raw).expect("a held block decodes");
        Self { block: Arc::new(block), raw }
    }

    fn hash(&self) -> BlockHash {
        self.block.header().hash
    }
}

impl Node {
    /// Mined, conflicting and orphaned entries leave the mempool
    fn adopt(&mut self, followed: Followed) {
        self.chain = followed;
        self.readmit();
    }

    /// Each entry re-admitted in order on its tip, refused ones dropped
    fn readmit(&mut self) {
        let held = std::mem::take(&mut self.mempool);
        let candidates = held.into_iter().map(|unmined| (unmined.tx, unmined.bytes));
        self.mempool = self.chain.ledger.clone().readmit(candidates);
    }

    /// Its tip's ledger + every entry: what a new transaction may spend
    fn pool(&self) -> Ledger {
        let mut pool = self.chain.ledger.clone();
        for unmined in &self.mempool {
            pool.admit(&unmined.tx).expect("entries admitted in order (readmit)");
        }
        pool
    }

    fn held(&self, txid: TxId) -> Option<&Unmined> {
        self.mempool.iter().find(|unmined| unmined.tx.txid == txid)
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

// ---- lies: what a lying node's `getblock` bytes say ----

/// How `getblock` misanswers (each caught by a body check)
///
/// - `WrongBlock` = the asked block re-mined under another nonce
/// - `Poisoned` = asked header + one extra transaction
/// - `Mutated` = last tx repeated (CVE-2012-2459)
/// - `WrongHeight` = the coinbase's BIP 34 height one up (v5: same txid, so same merkle root)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lie {
    WrongBlock,
    Poisoned,
    Mutated,
    WrongHeight,
}

impl Lie {
    /// This lie told about the block bytes `honest`
    pub fn told(self, honest: &[u8]) -> Vec<u8> {
        let (header, _) = HeaderBytes::split(honest).expect("a block's header");
        let mut header = header.as_bytes().to_vec();
        let mut txs: Vec<Vec<u8>> =
            crate::decode::tx_spans(honest).into_iter().map(|span| honest[span].to_vec()).collect();
        match self {
            // nonce = header bytes 108..140
            Lie::WrongBlock => header[139] ^= 0xff,
            Lie::Poisoned => txs.push(raw_transaction(0xbad, 0).1),
            Lie::Mutated => txs.push(txs.last().expect("a coinbase").clone()),
            Lie::WrongHeight => {
                let height = crate::decode_block(honest).expect("an honest block").header().height;
                txs[0] = coinbase_at(&txs[0], u32::from(height) + 1);
            }
        }
        let mut raw = header;
        CompactSize::write(&mut raw, txs.len()).expect("a Vec takes every write");
        txs.iter().for_each(|tx| raw.extend_from_slice(tx));
        raw
    }
}

/// `coinbase` rewritten to claim `height` (BIP 34 push swapped, the rest of its script kept)
fn coinbase_at(coinbase: &[u8], height: u32) -> Vec<u8> {
    let tx = CompressedTransaction::read(coinbase, BranchId::Sprout).expect("a coinbase");
    let data = tx.into_data();
    let transparent = data.transparent_bundle().expect("a coinbase input");
    let input = &transparent.vin[0];
    let script = &input.script_sig().0 .0;
    let push = usize::from(script[0]);
    let rest = if (1..=4).contains(&push) { &script[1 + push..] } else { &script[1..] };
    let script = [mint::bip34(height), rest.to_vec()].concat();
    let vin =
        vec![TxIn::from_parts(input.prevout().clone(), Script(Code(script)), input.sequence())];
    let transparent =
        Some(TransparentBundle { vin, vout: transparent.vout.clone(), ..transparent.clone() });
    let rewritten = match data.version() {
        TxVersion::V6 => CompressedTransactionData::from_parts_v6(
            data.consensus_branch_id(),
            data.lock_time(),
            data.expiry_height(),
            transparent,
            data.sapling_bundle().cloned(),
            data.orchard_bundle().cloned(),
            data.ironwood_bundle().cloned(),
        ),
        version => CompressedTransactionData::from_parts(
            version,
            data.consensus_branch_id(),
            data.lock_time(),
            data.expiry_height(),
            transparent,
            data.sprout_bundle().cloned(),
            data.sapling_bundle().cloned(),
            data.orchard_bundle().cloned(),
        ),
    };
    let mut raw = Vec::new();
    rewritten.freeze().expect("hashes").write(&mut raw).expect("a Vec takes every write");
    raw
}

#[cfg(test)]
mod tests;
