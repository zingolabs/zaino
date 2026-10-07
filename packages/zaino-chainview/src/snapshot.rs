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
use zaino_primitives::types::{BlockRef, BlockchainInfo, ReorgDepth, TransactionId, Zatoshis};

use crate::endpoints::{EndpointIndex, EndpointSet, EndpointState, ValidatorMetadata};
use crate::holders::Holders;
use crate::peers::{Overheard, Pending};
use crate::telemetry::Alarms;
use crate::tip::{ChainTip, Unserved};

/// One unconfirmed transaction, as served
///
/// - `fee` = a validator's listing (it resolved the prevouts); `None` = our own broadcast, not
///   yet listed by any validator
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

/// How far one transaction has spread (§5 `peers: x/y, trusted: x/y`), and how it got there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spread {
    pub peers: Count,
    pub trusted: Count,
    pub ours: bool,
    pub servable: bool,
    pub(crate) timeline: Timeline,
}

/// One transaction and where it has been seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sighting {
    trusted: EndpointSet,
    /// Every peer that announced it (live or not: `spread` counts the live ones)
    peers: OrdSet<SocketAddr>,
    ours: bool,
    raw: Bytes,
    fee: Option<Zatoshis>,
    projection: Projection,
    timeline: Timeline,
}

impl Sighting {
    /// `overheard` = peers' announcements from before the view held it
    pub(crate) fn new(
        raw: Bytes,
        fee: Option<Zatoshis>,
        ours: bool,
        overheard: Option<Pending>,
    ) -> Self {
        let projection = Projection::default();
        let now = Instant::now();
        let first_seen = overheard.as_ref().map_or(now, |pending| pending.first.min(now));
        let timeline = Timeline { first_seen, first_trusted: None, all_trusted: None };
        let peers = overheard.map(|pending| pending.peers).unwrap_or_default();
        Self { trusted: EndpointSet::default(), peers, ours, raw, fee, projection, timeline }
    }

    /// `true` = a new announcer
    pub(crate) fn hear(&mut self, peer: SocketAddr) -> bool {
        self.peers.insert(peer).is_none()
    }

    pub(crate) fn announcers(&self) -> &OrdSet<SocketAddr> {
        &self.peers
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

    fn spread(&self, readers: EndpointSet, live: &OrdSet<SocketAddr>) -> Spread {
        let heard = self.peers.iter().filter(|peer| live.contains(*peer)).count();
        Spread {
            peers: Count { seen: heard, of: live.len() },
            trusted: Count { seen: self.trusted.count(), of: readers.count() },
            ours: self.ours,
            servable: self.servable(),
            timeline: self.timeline,
        }
    }

    /// First listed fee kept (a fee = f(tx, its prevouts): every validator lists the same one)
    ///
    /// - priced at last → fresh projection (the old one rendered `fee: None`)
    pub(crate) fn listed_fee(&mut self, fee: Zatoshis) {
        if self.fee.is_none() {
            self.fee = Some(fee);
            self.projection = Projection::default();
        }
    }

    fn entry(&self, txid: TransactionId) -> MempoolEntry {
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

    /// Listed by any trusted validator (each admits only after full validation), or ours
    pub(crate) fn servable(&self) -> bool {
        self.ours || !self.trusted.is_empty()
    }

    pub(crate) fn sight(&mut self, endpoint: EndpointIndex) {
        self.trusted.insert(endpoint);
        self.timeline.first_trusted.get_or_insert_with(Instant::now);
    }

    pub(crate) fn unsight(&mut self, endpoint: EndpointIndex) {
        self.trusted.remove(endpoint);
    }

    pub(crate) fn mark_ours(&mut self) {
        self.ours = true;
    }
}

/// One coherent answer: the verified tip, the mempool, every trusted validator's metadata
///
/// [`tip`](Self::tip) is `None` without a verified best block a trusted validator holds — no
/// answer rather than a weak one — and [`mempool`](Self::mempool) refuses on the same condition.
#[derive(Debug, Clone)]
pub struct ChainViewSnapshot {
    /// The verified chain + who holds what of it
    pub(crate) holders: Holders,
    pub(crate) tip: Option<ChainTip>,
    /// Ordered, so two readers of one view walk the mempool identically.
    pub(crate) mempool: OrdMap<TransactionId, Sighting>,
    pub(crate) endpoints: Vector<ValidatorMetadata>,
    pub(crate) alarms: Alarms,
    pub(crate) finality_paused: bool,
    /// Connected peers as of the last peer fold (`peers: x/y`'s `y`)
    pub(crate) peers_live: OrdSet<SocketAddr>,
    pub(crate) overheard: Overheard,
}

impl ChainViewSnapshot {
    pub(crate) fn empty(endpoints: Vector<ValidatorMetadata>, depth: ReorgDepth) -> Self {
        Self {
            holders: Holders::new(endpoints.len(), depth),
            tip: None,
            mempool: OrdMap::new(),
            endpoints,
            alarms: Alarms::default(),
            finality_paused: false,
            peers_live: OrdSet::new(),
            overheard: Overheard::default(),
        }
    }

    /// Consumers' tests: `chain` as header sync's word, `held_by` holding its best, one fresh
    /// endpoint per address; `ours` servable (our relay), `unlisted` held, not servable
    #[cfg(any(test, feature = "testing"))]
    pub fn fixed(
        chain: Option<Arc<VerifiedChain>>,
        held_by: EndpointSet,
        addresses: &[&str],
        ours: &[(TransactionId, Bytes)],
        unlisted: &[(TransactionId, Bytes)],
    ) -> Self {
        let endpoints = addresses.iter().map(|at| ValidatorMetadata::new((*at).to_owned()));
        let mut view = Self::empty(endpoints.collect(), ReorgDepth::CONSENSUS);
        view.holders.verified(chain);
        let held = view.best().filter(|_| !held_by.is_empty());
        view.tip = held.map(|block| ChainTip { block, held_by });
        for (sightings, servable) in [(ours, true), (unlisted, false)] {
            for (txid, raw) in sightings {
                let sighting = Sighting::new(raw.clone(), None, servable, None);
                view.mempool.insert(*txid, sighting);
            }
        }
        view
    }

    /// Peers that announced `txid`, held or only overheard (submission's watch)
    pub(crate) fn announcers(&self, txid: &TransactionId) -> OrdSet<SocketAddr> {
        match (self.mempool.get(txid), self.overheard.get(txid)) {
            (Some(sighting), _) => sighting.announcers().clone(),
            (None, Some(pending)) => pending.peers.clone(),
            (None, None) => OrdSet::new(),
        }
    }

    /// The verified best block and its trusted holders; `None` = [`unserved`](Self::unserved)
    pub fn tip(&self) -> Option<ChainTip> {
        self.tip
    }

    /// The header chain's best block, whether or not a trusted validator holds it
    pub fn best(&self) -> Option<BlockRef> {
        self.holders.best()
    }

    /// Header sync's word every standing was judged against (`None` = nothing verified yet)
    pub fn chain(&self) -> Option<&Arc<VerifiedChain>> {
        self.holders.chain()
    }

    /// Servable here, not servable (or absent) in `since`, txid order (`None` = every servable)
    ///
    /// - `imbl` diff: shared subtrees skipped (O(changes) between consecutive publishes)
    pub fn arrivals(&self, since: Option<&ChainViewSnapshot>) -> Vec<MempoolEntry> {
        let empty = OrdMap::new();
        let before = since.map_or(&empty, |since| &since.mempool);
        let added = before.diff(&self.mempool).filter_map(|change| match change {
            DiffItem::Add(txid, now) => Some((*txid, now)),
            DiffItem::Update { old: (_, was), new: (txid, now) } => {
                (!was.servable()).then_some((*txid, now))
            }
            DiffItem::Remove(..) => None,
        });
        let servable = added.filter(|(_, sighting)| sighting.servable());
        servable.map(|(txid, sighting)| sighting.entry(txid)).collect()
    }

    /// Why there is no tip (`None` = there is one)
    pub fn unserved(&self) -> Option<Unserved> {
        match (self.tip, self.best()) {
            (Some(_), _) => None,
            (None, None) => Some(Unserved::NoBestTip),
            (None, Some(best)) => Some(Unserved::NotHeld {
                height: u32::from(best.height),
                configured: self.endpoints.len(),
            }),
        }
    }

    fn served(&self) -> Result<ChainTip, Unserved> {
        self.tip.ok_or_else(|| self.unserved().unwrap_or(Unserved::NoBestTip))
    }

    /// Partition / eclipse / stale-tip conditions as of the last fold (telemetry only, never gates)
    pub fn alarms(&self) -> Alarms {
        self.alarms
    }

    /// Per-endpoint metadata, in configured order
    pub fn endpoints(&self) -> &Vector<ValidatorMetadata> {
        &self.endpoints
    }

    /// `getblockchaininfo` of the first trusted validator holding the tip
    ///
    /// - holders share the tip block → one schedule, one branch; every holder has stored one
    pub fn validator_info(&self) -> Result<&BlockchainInfo, Unserved> {
        let tip = self.served()?;
        let info = tip.held_by.positions().find_map(|at| self.endpoints.get(at)?.info.as_ref());
        info.ok_or(Unserved::NotHeld {
            height: u32::from(tip.block.height),
            configured: self.endpoints.len(),
        })
    }

    /// Where one transaction has been seen, regardless of whether it is servable.
    ///
    /// Telemetry and the action stream read this; the mempool RPCs go through
    /// [`mempool`](Self::mempool).
    pub(crate) fn sighting(&self, txid: &TransactionId) -> Option<&Sighting> {
        self.mempool.get(txid)
    }

    /// Trusted validators whose mempool is read now (`Live`: catching-up and down ones list none)
    pub fn mempool_readers(&self) -> EndpointSet {
        self.endpoints
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.state == EndpointState::Live)
            .filter_map(|(position, _)| EndpointIndex::new(position))
            .collect()
    }

    /// One transaction's spread, servable or not (telemetry; never gates serving)
    pub fn spread(&self, txid: &TransactionId) -> Option<Spread> {
        Some(self.mempool.get(txid)?.spread(self.mempool_readers(), &self.peers_live))
    }

    /// Every held transaction's spread, in txid order
    pub fn spreads(&self) -> impl Iterator<Item = (TransactionId, Spread)> + '_ {
        let readers = self.mempool_readers();
        self.mempool
            .iter()
            .map(move |(txid, sighting)| (*txid, sighting.spread(readers, &self.peers_live)))
    }

    /// The mempool, or the refusal that stands in for it without a tip
    ///
    /// A `Result` rather than an empty answer: with no tip there is no honest answer to give,
    /// and a caller must not be able to forget that (§5, fail closed).
    pub fn mempool(&self) -> Result<MempoolView<'_>, Unserved> {
        self.served().map(|_| MempoolView(self))
    }
}

/// The servable mempool of a snapshot that has a tip.
///
/// Every method here applies the per-transaction rule — listed by any validator, or `ours` —
/// so nothing below it can leak out.
#[derive(Debug, Clone, Copy)]
pub struct MempoolView<'a>(&'a ChainViewSnapshot);

impl<'a> MempoolView<'a> {
    /// Callers checked `snapshot.mempool()` already (a tail's anchor)
    pub(crate) fn of(snapshot: &'a ChainViewSnapshot) -> Self {
        Self(snapshot)
    }

    /// One servable unconfirmed transaction.
    pub(crate) fn get(&self, txid: &TransactionId) -> Option<MempoolEntry> {
        self.0
            .mempool
            .get(txid)
            .filter(|sighting| sighting.servable())
            .map(|sighting| sighting.entry(*txid))
    }

    /// Every servable unconfirmed transaction, in txid order.
    pub fn entries(&self) -> impl Iterator<Item = MempoolEntry> + '_ {
        self.0
            .mempool
            .iter()
            .filter(|(_, sighting)| sighting.servable())
            .map(|(txid, sighting)| sighting.entry(*txid))
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
