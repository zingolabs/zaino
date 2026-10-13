//! The published view: verified tip, sighted transactions, per-endpoint metadata.
//!
//! - `imbl` collections, so republishing on every fold clones in `O(1)` with structural sharing
//! - reader takes **one `ArcSwap` load per request or stream**, pinning a coherent view — a fold
//!   landing mid-stream cannot splice two views into one response
//! - raw bytes only, never a decoded transaction (parsing = the wire adapter's job, and
//!   `zaino-proto` must not reach this crate)

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use imbl::ordmap::DiffItem;
use imbl::{OrdMap, OrdSet, Vector};
use tokio::time::Instant;
use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{
    BlockHash, BlockRef, BlockchainInfo, ReorgDepth, TransactionId, Zatoshis,
};
use zaino_traffic::{Health, ValidatorId};

use crate::config::FINALITY_STALL;
use crate::endpoints::{Agreement, EndpointSet, ValidatorMetadata};
use crate::listings::Listings;
use crate::telemetry::{self, Alarms};

/// One unconfirmed transaction, as served
///
/// - `fee` = a validator's listing, or ours from the served UTXO set (a peer quorum's); `None` =
///   our own broadcast, not yet listed or admitted
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolEntry {
    pub txid: TransactionId,
    pub raw: Bytes,
    pub fee: Option<Zatoshis>,
    pub projection: Projection,
}

/// Serving layer's encoding of one (`raw`, `fee`), rendered once and shared by every snapshot
/// holding the sighting (`GetMempoolTx` = consensus parse per tx otherwise, per request)
///
/// - cache, never identity: always equal
#[derive(Debug, Clone, Default)]
pub struct Projection(Arc<OnceLock<Bytes>>);

impl Projection {
    /// Racing first readers may both render (same bytes; one kept); a failed render is not kept
    pub fn get_or_render<E>(&self, render: impl FnOnce() -> Result<Bytes, E>) -> Result<Bytes, E> {
        if let Some(rendered) = self.0.get() {
            return Ok(rendered.clone());
        }
        let rendered = render()?;
        Ok(self.0.get_or_init(|| rendered).clone())
    }
}

impl PartialEq for Projection {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Projection {}

/// `seen` of the `of` sources whose mempool is being read right now
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub seen: usize,
    pub of: usize,
}

impl std::fmt::Display for Count {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.seen, self.of)
    }
}

/// When one transaction reached each milestone (tokio clock: paused tests advance it)
///
/// - `all_trusted` = first moment every mempool-reading trusted validator listed it at once
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timeline {
    pub(crate) first_seen: Instant,
    pub(crate) first_trusted: Option<Instant>,
    pub(crate) all_trusted: Option<Instant>,
}

/// How far one transaction has spread (`peers: x/y, trusted: x/y`), and how it got there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spread {
    pub peers: Count,
    pub trusted: Count,
    pub quorum: bool,
    pub ours: bool,
    pub(crate) timeline: Timeline,
}

/// One transaction the view serves, and where it has been seen
///
/// - `quorum` = a peer quorum admitted it (`listings.rs`) and it passed our checks
/// - always listed (trusted or quorum) or ours: a fold drops one that is neither before publishing
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sighting {
    trusted: EndpointSet,
    quorum: bool,
    ours: bool,
    raw: Bytes,
    fee: Option<Zatoshis>,
    projection: Projection,
    timeline: Timeline,
}

impl Sighting {
    pub(crate) fn new(raw: Bytes, fee: Option<Zatoshis>, ours: bool) -> Self {
        let projection = Projection::default();
        let timeline =
            Timeline { first_seen: Instant::now(), first_trusted: None, all_trusted: None };
        Self {
            trusted: EndpointSet::default(),
            quorum: false,
            ours,
            raw,
            fee,
            projection,
            timeline,
        }
    }

    pub(crate) fn raw(&self) -> &Bytes {
        &self.raw
    }

    pub(crate) fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// Records `all_trusted` the first time `readers` (non-empty) all list it; `true` = now
    pub(crate) fn reached_all(&mut self, readers: EndpointSet) -> bool {
        let everywhere = !readers.is_empty() && self.trusted.covers(readers);
        let first = everywhere && self.timeline.all_trusted.is_none();
        if first {
            self.timeline.all_trusted = Some(Instant::now());
        }
        first
    }

    /// `listers` = fresh peers listing it, of `polled`
    fn spread(&self, readers: EndpointSet, listers: usize, polled: usize) -> Spread {
        Spread {
            peers: Count { seen: listers, of: polled },
            trusted: Count { seen: self.trusted.count(), of: readers.count() },
            quorum: self.quorum,
            ours: self.ours,
            timeline: self.timeline,
        }
    }

    /// First known fee kept (a fee = f(tx, its prevouts): every validator lists the same one)
    ///
    /// - priced at last → fresh projection (the old one rendered `fee: None`)
    pub(crate) fn listed_fee(&mut self, fee: Zatoshis) {
        if self.fee.is_none() {
            self.fee = Some(fee);
            self.projection = Projection::default();
        }
    }

    pub(crate) fn entry(&self, txid: TransactionId) -> MempoolEntry {
        let projection = self.projection.clone();
        MempoolEntry { txid, raw: self.raw.clone(), fee: self.fee, projection }
    }

    /// Trusted validators listing it now
    pub(crate) fn trusted(&self) -> EndpointSet {
        self.trusted
    }

    /// Relayed by us, so known before it propagated anywhere.
    pub(crate) fn ours(&self) -> bool {
        self.ours
    }

    /// Listed by a trusted validator (each admits only after full validation), or admitted by a
    /// peer quorum
    pub(crate) fn listed(&self) -> bool {
        !self.trusted.is_empty() || self.quorum
    }

    pub(crate) fn quorum(&self) -> bool {
        self.quorum
    }

    /// Admitted by a peer quorum (`true`) or no longer retained by one (`false`); `true` = changed
    pub(crate) fn set_quorum(&mut self, quorum: bool) -> bool {
        std::mem::replace(&mut self.quorum, quorum) != quorum
    }

    pub(crate) fn sight(&mut self, endpoint: ValidatorId) {
        self.trusted.insert(endpoint);
        self.timeline.first_trusted.get_or_insert_with(Instant::now);
    }

    pub(crate) fn unsight(&mut self, endpoint: ValidatorId) {
        self.trusted.remove(endpoint);
    }

    pub(crate) fn mark_ours(&mut self) {
        self.ours = true;
    }
}

/// One coherent answer: the verified chain, its holders, the mempool, every trusted validator's
/// metadata
///
/// - [`mempool`](Self::mempool) `None` while no live trusted validator holds the verified best
///   and no peer quorum reads (no answer rather than a weak one)
/// - `held_by` = `Live` validators whose last poll holds the best block, as of the last fold
/// - `owed_since` = since when a block `depth` deep waits, final tip unmoved (stall alarm)
/// - `mempool` ordered: two readers of one view walk it identically
/// - `peers_live` = connected peers as of the last block-`inv` fold
/// - `peers_carrying` = each live peer's latest announced block (peer finality's netgroup count)
/// - `listings` = each live peer's last mempool listing (the quorum; `peers: x/y`)
#[derive(Debug, Clone)]
pub struct ChainViewSnapshot {
    pub(crate) chain: Option<Arc<VerifiedChain>>,
    pub(crate) depth: ReorgDepth,
    pub(crate) owed_since: Option<Instant>,
    pub(crate) held_by: EndpointSet,
    pub(crate) mempool: OrdMap<TransactionId, Sighting>,
    pub(crate) endpoints: Vector<ValidatorMetadata>,
    pub(crate) alarms: Alarms,
    pub(crate) peers_live: OrdSet<SocketAddr>,
    pub(crate) peers_carrying: OrdMap<SocketAddr, BlockHash>,
    pub(crate) listings: Listings,
}

impl ChainViewSnapshot {
    pub(crate) fn empty(endpoints: Vector<ValidatorMetadata>, depth: ReorgDepth) -> Self {
        Self {
            chain: None,
            depth,
            owed_since: None,
            held_by: EndpointSet::default(),
            mempool: OrdMap::new(),
            endpoints,
            alarms: Alarms::default(),
            peers_live: OrdSet::new(),
            peers_carrying: OrdMap::new(),
            listings: Listings::default(),
        }
    }

    /// Header sync's word: `owed_since` restarts when the final tip moves, ends when nothing is
    /// owed (≤ `depth` unfinal on the best)
    pub(crate) fn verified(&mut self, chain: Option<Arc<VerifiedChain>>, now: Instant) {
        let final_tip = |chain: &Option<Arc<VerifiedChain>>| chain.as_ref().map(|c| c.final_tip());
        let moved = final_tip(&chain) != final_tip(&self.chain);
        let owed = chain.as_deref().is_some_and(|chain| {
            let base = u32::from(chain.final_tip().height) + 1;
            (u32::from(chain.best().height) + 1).saturating_sub(base) > self.depth.get()
        });
        self.owed_since = match (owed, moved) {
            (false, _) => None,
            (true, true) => Some(now),
            (true, false) => self.owed_since.or(Some(now)),
        };
        self.chain = chain;
    }

    /// A block `depth` deep owed finality ≥ [`FINALITY_STALL`] (unvouched, peers short of it, H6)
    pub(crate) fn finality_stalled(&self, now: Instant) -> bool {
        self.owed_since.is_some_and(|since| now.duration_since(since) >= FINALITY_STALL)
    }

    /// Consumers' tests: `chain` as header sync's word, `held_by` holding its best, one fresh
    /// endpoint per address; `ours` served (our relay)
    ///
    /// - each holder `Live` + `Agreed`, its `getblockchaininfo` = the best block, no upgrade,
    ///   branch id 0
    #[cfg(any(test, feature = "testing"))]
    pub fn fixed(
        chain: Option<Arc<VerifiedChain>>,
        held_by: EndpointSet,
        addresses: &[&str],
        ours: &[(TransactionId, Bytes)],
    ) -> Self {
        use zaino_primitives::types::{ConsensusBranchId, ConsensusBranchIds, Height};

        let endpoints = addresses.iter().map(|at| ValidatorMetadata::new((*at).to_owned()));
        let mut view = Self::empty(endpoints.collect(), ReorgDepth::CONSENSUS);
        view.chain = chain;
        view.held_by = view.best().map_or_else(EndpointSet::default, |_| held_by);
        if let Some(best) = view.best() {
            let branch = ConsensusBranchId::new(0);
            let info = BlockchainInfo {
                blocks: best.height,
                estimated_height: best.height,
                best_block_hash: best.hash,
                sapling_activation: Height::GENESIS,
                upgrades: Vec::new(),
                consensus: ConsensusBranchIds { chain_tip: branch, next_block: branch },
            };
            for at in view.held_by.positions() {
                if let Some(meta) = view.endpoints.get_mut(at) {
                    meta.info = Some(info.clone());
                    meta.health = Health::Live;
                    meta.agreement = Agreement::Agreed;
                }
            }
        }
        for (txid, raw) in ours {
            view.mempool.insert(*txid, Sighting::new(raw.clone(), None, true));
        }
        view
    }

    /// Consumers' tests: endpoint `at`'s health, and its clock estimate `behind` blocks past
    /// its tip (a [`fixed`](Self::fixed) holder's tip = best); alarms re-derived as a fold would
    #[cfg(any(test, feature = "testing"))]
    pub fn endpoint(mut self, at: usize, health: Health, behind: u32) -> Self {
        if let Some(meta) = self.endpoints.get_mut(at) {
            meta.health = health;
            if let Some(info) = meta.info.as_mut() {
                info.estimated_height =
                    info.blocks.checked_add(behind).expect("estimate below u32::MAX");
            }
        }
        self.alarms = crate::telemetry::alarms(&self.endpoints, self.alarms.finality_paused());
        self
    }

    /// Consumers' tests: holder `at` claims `by` blocks past best (header sync short of its tip)
    ///
    /// - `read` = its hash at best polled: `Ahead`, still a holder; else no poll since the chain
    ///   moved: `Diverged`, no longer a holder (as a fold would)
    #[cfg(any(test, feature = "testing"))]
    pub fn ahead(mut self, at: usize, by: u32, read: bool) -> Self {
        let best = self.best().expect("ahead of a verified best");
        let id = ValidatorId::new(at).expect("a small position");
        assert!(self.held_by.contains(id), "{at}: not a holder");
        let meta = self.endpoints.get_mut(at).expect("a holder = a configured endpoint");
        let info = meta.info.as_mut().expect("a holder answered");
        info.blocks = best.height.checked_add(by).expect("claim below u32::MAX");
        info.estimated_height = info.blocks;
        info.best_block_hash = BlockHash::from([0xA0; 32]);
        meta.held = read.then_some(best);
        meta.agreement = meta.agreement_with(self.chain.as_deref());
        if !read {
            self.held_by.remove(id);
        }
        self
    }

    /// An answering validator claims past best, not shown off our chain (best ≠ the network's
    /// tip yet: header sync short of it, e.g. anchored `depth` below its claim at boot)
    pub fn claimed_past_best(&self) -> bool {
        let Some(chain) = self.chain.as_deref() else { return false };
        self.endpoints.iter().any(|meta| meta.claims_past(chain))
    }

    /// Answering validators (`Live` or catching up) whose last poll holds the verified best block
    /// (empty = no chain yet)
    pub fn held_by(&self) -> EndpointSet {
        self.held_by
    }

    /// The header chain's best block, whether or not a trusted validator holds it
    pub fn best(&self) -> Option<BlockRef> {
        self.chain.as_ref().map(|chain| chain.best())
    }

    /// Header sync's word every standing was judged against (`None` = nothing verified yet)
    pub fn chain(&self) -> Option<&Arc<VerifiedChain>> {
        self.chain.as_ref()
    }

    /// Held here, absent in `since`, txid order (`None` = every one held)
    ///
    /// - `imbl` diff: shared subtrees skipped (O(changes) between consecutive publishes)
    pub fn arrivals(&self, since: Option<&ChainViewSnapshot>) -> Vec<MempoolEntry> {
        let empty = OrdMap::new();
        let before = since.map_or(&empty, |since| &since.mempool);
        let added = before.diff(&self.mempool).filter_map(|change| match change {
            DiffItem::Add(txid, now) => Some(now.entry(*txid)),
            DiffItem::Update { .. } | DiffItem::Remove(..) => None,
        });
        added.collect()
    }

    /// Partition / eclipse / stale-tip conditions as of the last fold (telemetry only, never gates)
    pub fn alarms(&self) -> Alarms {
        self.alarms
    }

    /// Per-endpoint metadata, in configured order
    pub fn endpoints(&self) -> &Vector<ValidatorMetadata> {
        &self.endpoints
    }

    /// `getblockchaininfo` of the first trusted validator holding the best block (`None` = none
    /// holds it)
    ///
    /// - holders share the best block → one schedule, one branch
    pub fn validator_info(&self) -> Option<&BlockchainInfo> {
        self.held_by.positions().find_map(|at| self.endpoints.get(at)?.info.as_ref())
    }

    /// Fewest outbound peers any two live validators share (`None` below two reporting any)
    pub fn shared_outbound_min(&self) -> Option<usize> {
        telemetry::shared_outbound_min(&telemetry::outbound(&self.endpoints))
    }

    /// Where one held transaction has been seen, behind no gate (telemetry, submission's watch;
    /// the mempool RPCs go through [`mempool`](Self::mempool))
    pub(crate) fn sighting(&self, txid: &TransactionId) -> Option<&Sighting> {
        self.mempool.get(txid)
    }

    /// Trusted validators whose mempool is read now (`Live`: catching-up and down ones list none)
    pub fn mempool_readers(&self) -> EndpointSet {
        self.endpoints
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.health == Health::Live)
            .filter_map(|(position, _)| ValidatorId::new(position))
            .collect()
    }

    /// One held transaction's spread (telemetry; never gates serving)
    pub fn spread(&self, txid: &TransactionId) -> Option<Spread> {
        let polled = self.listings.polled().len();
        let listers = self.listings.listers(txid).len();
        Some(self.mempool.get(txid)?.spread(self.mempool_readers(), listers, polled))
    }

    /// Every held transaction's spread, in txid order
    pub fn spreads(&self) -> impl Iterator<Item = (TransactionId, Spread)> + '_ {
        let (readers, polled) = (self.mempool_readers(), self.listings.polled().len());
        self.mempool.iter().map(move |(txid, sighting)| {
            (*txid, sighting.spread(readers, self.listings.listers(txid).len(), polled))
        })
    }

    /// The served mempool; `None` while neither a `Live` trusted validator (its mempool
    /// listing) holds the best block nor [`peers_quorate`](Self::peers_quorate) (fail closed: no
    /// empty stand-in)
    pub fn mempool(&self) -> Option<MempoolView<'_>> {
        let live = |at: usize| self.endpoints.get(at).is_some_and(|m| m.health == Health::Live);
        let trusted = self.held_by.positions().any(live);
        (trusted || self.peers_quorate()).then_some(MempoolView(self))
    }

    /// Fresh peer mempool listings span enough netgroups for a quorum (`listings.rs`)
    pub fn peers_quorate(&self) -> bool {
        self.listings.quorate()
    }
}

/// The mempool of a snapshot a trusted validator or a peer quorum reads
#[derive(Debug, Clone, Copy)]
pub struct MempoolView<'a>(&'a ChainViewSnapshot);

impl MempoolView<'_> {
    /// Every unconfirmed transaction held (listed by a trusted validator, admitted by a peer
    /// quorum, or ours), in txid order
    pub fn entries(&self) -> impl Iterator<Item = MempoolEntry> + '_ {
        self.0.mempool.iter().map(|(txid, sighting)| sighting.entry(*txid))
    }

    /// `GetMempoolTx`'s filter: everything except the transactions a suffix identifies.
    ///
    /// `service.proto:293-305` — a suffix is matched against the txid's **protocol-order** bytes
    /// (a truncated hex txid reverses into a byte suffix), and a suffix matching two or more
    /// entries excludes none of them (ambiguous ⇒ the client keeps receiving both). A suffix
    /// matching nothing is ignored; an empty suffix matches everything, so it excludes only a
    /// single-entry mempool — fallout of the same rule, not a special case.
    ///
    /// `O(suffixes × entries)`, both mempool-bounded.
    pub fn excluding<B: AsRef<[u8]>>(&self, suffixes: &[B]) -> Vec<MempoolEntry> {
        let servable: Vec<MempoolEntry> = self.entries().collect();
        let mut excluded: Vec<TransactionId> = Vec::new();

        for suffix in suffixes {
            let mut matched =
                servable.iter().filter(|entry| ends_with(&entry.txid, suffix.as_ref()));
            if let (Some(only), None) = (matched.next(), matched.next()) {
                excluded.push(only.txid);
            }
        }

        servable.into_iter().filter(|entry| !excluded.contains(&entry.txid)).collect()
    }
}

fn ends_with(txid: &TransactionId, suffix: &[u8]) -> bool {
    <[u8; 32]>::from(*txid).ends_with(suffix)
}
