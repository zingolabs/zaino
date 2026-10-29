//! Published view: verified tip, sighted transactions, per-validator metadata
//!
//! - `imbl` collections (republish per merge = `O(1)` clone, structural sharing)
//! - reader: one `ArcSwap` load per request or stream (a mid-stream merge never splices two views)
//! - raw bytes only, never decoded (parsing = the wire adapter's, `zaino-proto` never reaches here)

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use imbl::ordmap::DiffItem;
use imbl::{OrdMap, OrdSet, Vector};
use tokio::time::Instant;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_traffic::{Health, TrustedValidatorId};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{BlockHash, TxId};

use crate::alarm_thresholds::FINALITY_PAUSE;
use crate::listings::Listings;
use crate::telemetry::Alarms;
use crate::trusted_validators::{Agreement, TrustedValidatorMeta, TrustedValidatorSet};

/// One unconfirmed transaction, as served
///
/// - `fee` = a validator's listing, or ours from the served UTXO set (a peer quorum's)
/// - `fee: None` = our own broadcast, not yet listed or admitted
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolEntry {
    pub txid: TxId,
    pub raw: Bytes,
    pub fee: Option<Zatoshis>,
    pub projection: Projection,
}

/// Serving layer's encoding of one (`raw`, `fee`): rendered once, shared per `ChainViewSnapshot`
/// holding the sighting (`GetMempoolTx` = consensus parse per tx otherwise, per request)
///
/// - cache, never identity: always equal
#[derive(Debug, Clone, Default)]
pub struct Projection(Arc<OnceLock<Bytes>>);

impl Projection {
    /// - racing first readers may both render (same bytes, one kept)
    /// - failed render never kept
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

/// Why one held transaction is held, first match: a trusted listing, a peer quorum, our relay
///
/// - `Ours` = neither listing (a merge drops one not ours)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolState {
    Verified,
    Quorum,
    Ours,
}

impl MempoolState {
    pub(crate) fn of(trusted: TrustedValidatorSet, quorum: bool) -> Self {
        match (trusted.is_empty(), quorum) {
            (false, _) => Self::Verified,
            (true, true) => Self::Quorum,
            (true, false) => Self::Ours,
        }
    }

    /// Listed by a trusted validator (each admits only after full validation), or admitted by a
    /// peer quorum
    pub(crate) fn listed(self) -> bool {
        self != Self::Ours
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Quorum => "quorum",
            Self::Ours => "ours_unverified",
        }
    }
}

/// One transaction the view serves, and where it has been seen
///
/// - `quorum` = a peer quorum admitted it (`listings.rs`) and it passed our checks
/// - always listed (trusted or quorum) or ours: a merge drops one that is neither before publishing
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sighting {
    trusted: TrustedValidatorSet,
    quorum: bool,
    ours: bool,
    raw: Bytes,
    fee: Option<Zatoshis>,
    projection: Projection,
}

impl Sighting {
    /// Listed (trusted or quorum) with its fee
    pub(crate) fn priced(raw: Bytes, fee: Zatoshis) -> Self {
        let (trusted, projection) = (TrustedValidatorSet::default(), Projection::default());
        Self { trusted, quorum: false, ours: false, raw, fee: Some(fee), projection }
    }

    /// Relayed by us, before any listing prices it
    pub(crate) fn relayed(raw: Bytes) -> Self {
        let (trusted, projection) = (TrustedValidatorSet::default(), Projection::default());
        Self { trusted, quorum: false, ours: true, raw, fee: None, projection }
    }

    pub(crate) fn raw(&self) -> &Bytes {
        &self.raw
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

    pub(crate) fn entry(&self, txid: TxId) -> MempoolEntry {
        let projection = self.projection.clone();
        MempoolEntry { txid, raw: self.raw.clone(), fee: self.fee, projection }
    }

    /// Trusted validators listing it now
    pub(crate) fn trusted(&self) -> TrustedValidatorSet {
        self.trusted
    }

    /// Relayed by us (known before it propagated anywhere)
    pub(crate) fn ours(&self) -> bool {
        self.ours
    }

    pub(crate) fn state(&self) -> MempoolState {
        MempoolState::of(self.trusted, self.quorum)
    }

    pub(crate) fn quorum(&self) -> bool {
        self.quorum
    }

    /// Admitted by a peer quorum (`true`) or no longer retained by one (`false`) → `true` = changed
    pub(crate) fn set_quorum(&mut self, quorum: bool) -> bool {
        std::mem::replace(&mut self.quorum, quorum) != quorum
    }

    pub(crate) fn sight(&mut self, validator: TrustedValidatorId) {
        self.trusted.insert(validator);
    }

    pub(crate) fn unsight(&mut self, validator: TrustedValidatorId) {
        self.trusted.remove(validator);
    }

    pub(crate) fn mark_ours(&mut self) {
        self.ours = true;
    }
}

/// One coherent answer: header chain, mempool, each trusted validator's metadata
///
/// - `peers_carrying` = each live peer's latest announced block (header sync's peer witnesses)
#[derive(Debug, Clone)]
pub struct ChainViewSnapshot {
    pub(crate) chain: Option<Arc<HeaderChainSnapshot>>,
    pub(crate) depth: FinalityDepth,
    pub(crate) owed_since: Option<Instant>,
    pub(crate) mempool: OrdMap<TxId, Sighting>,
    pub(crate) trusted_validators: Vector<TrustedValidatorMeta>,
    pub(crate) alarms: Alarms,
    pub(crate) peers_live: OrdSet<SocketAddr>,
    pub(crate) peers_carrying: OrdMap<SocketAddr, BlockHash>,
    pub(crate) listings: Listings,
}

impl ChainViewSnapshot {
    pub(crate) fn empty(
        trusted_validators: Vector<TrustedValidatorMeta>,
        depth: FinalityDepth,
    ) -> Self {
        Self {
            chain: None,
            depth,
            owed_since: None,
            mempool: OrdMap::new(),
            trusted_validators,
            alarms: Alarms::default(),
            peers_live: OrdSet::new(),
            peers_carrying: OrdMap::new(),
            listings: Listings::default(),
        }
    }

    /// Header sync's word: `owed_since` restarts when the finalized tip moves, ends when nothing is
    /// owed (≤ `depth` non-finalized on the best)
    pub(crate) fn set_chain(&mut self, chain: Option<Arc<HeaderChainSnapshot>>, now: Instant) {
        let finalized_tip =
            |chain: &Option<Arc<HeaderChainSnapshot>>| chain.as_ref().map(|c| c.finalized_tip());
        let moved = finalized_tip(&chain) != finalized_tip(&self.chain);
        let owed = chain.as_deref().is_some_and(|chain| {
            let base = u32::from(chain.finalized_tip().height) + 1;
            (u32::from(chain.best().height) + 1).saturating_sub(base) > self.depth.get()
        });
        self.owed_since = match (owed, moved) {
            (false, _) => None,
            (true, true) => Some(now),
            (true, false) => self.owed_since.or(Some(now)),
        };
        self.chain = chain;
    }

    /// A block `depth` deep owed finality ≥ [`FINALITY_PAUSE`] (unvouched, peers short of it, H6)
    pub(crate) fn finality_paused(&self, now: Instant) -> bool {
        self.owed_since.is_some_and(|since| now.duration_since(since) >= FINALITY_PAUSE)
    }

    /// An answering validator claims past best, not shown off our chain (best ≠ the network's
    /// tip yet: header sync short of it, e.g. anchored `depth` below its claim at boot)
    pub fn claimed_past_best(&self) -> bool {
        let Some(chain) = self.chain.as_deref() else { return false };
        self.trusted_validators.iter().any(|meta| meta.claims_past(chain))
    }

    /// Answering validators (`Live` or catching up) whose last poll holds the best block (empty =
    /// no chain yet)
    pub fn held_by(&self) -> TrustedValidatorSet {
        let Some(chain) = self.chain.as_deref() else { return TrustedValidatorSet::default() };
        let holding = self.trusted_validators.iter().enumerate().filter(|(_, m)| m.holds(chain));
        holding.filter_map(|(at, _)| TrustedValidatorId::new(at)).collect()
    }

    /// Each trusted validator's chain vs best, in configured order
    pub fn agreements(&self) -> impl Iterator<Item = Agreement> + '_ {
        self.trusted_validators.iter().map(|meta| meta.agreement_with(self.chain.as_deref()))
    }

    /// The header chain's best block, whether or not a trusted validator holds it
    pub fn best(&self) -> Option<BlockRef> {
        self.chain.as_ref().map(|chain| chain.best())
    }

    /// Header sync's word every standing was judged against (`None` = nothing verified yet)
    pub fn chain(&self) -> Option<&Arc<HeaderChainSnapshot>> {
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

    /// Raised alarms as of the last merge (telemetry only, never gates)
    pub fn alarms(&self) -> &Alarms {
        &self.alarms
    }

    /// Per-validator metadata, in configured order
    pub fn trusted_validators(&self) -> &Vector<TrustedValidatorMeta> {
        &self.trusted_validators
    }

    /// Where one held transaction has been seen, behind no gate (telemetry, submission's watch)
    ///
    /// - mempool RPCs → [`mempool`](Self::mempool)
    pub(crate) fn sighting(&self, txid: &TxId) -> Option<&Sighting> {
        self.mempool.get(txid)
    }

    /// Trusted validators whose mempool is read now (`Live`: catching-up and down ones list none)
    pub(crate) fn mempool_readers(&self) -> TrustedValidatorSet {
        self.trusted_validators
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.health == Health::Live)
            .filter_map(|(position, _)| TrustedValidatorId::new(position))
            .collect()
    }

    /// Every held transaction's [`MempoolState`], in txid order (telemetry, never gates serving)
    pub fn mempool_states(&self) -> impl Iterator<Item = MempoolState> + '_ {
        self.mempool.values().map(Sighting::state)
    }

    /// Served mempool
    ///
    /// - `None` while neither a `Live` trusted validator (its mempool listing) holds the best block
    ///   nor `peers_quorate()` (fail closed: no empty stand-in)
    pub fn mempool(&self) -> Option<MempoolView<'_>> {
        let live =
            |at: usize| self.trusted_validators.get(at).is_some_and(|m| m.health == Health::Live);
        let trusted = self.held_by().positions().any(live);
        (trusted || self.peers_quorate()).then_some(MempoolView(self))
    }

    /// Fresh peer mempool listings span enough netgroups for a quorum (`listings.rs`)
    pub(crate) fn peers_quorate(&self) -> bool {
        self.listings.quorate()
    }
}

/// Mempool of a `ChainViewSnapshot` a trusted validator or a peer quorum reads
#[derive(Debug, Clone, Copy)]
pub struct MempoolView<'a>(&'a ChainViewSnapshot);

impl MempoolView<'_> {
    /// Every unconfirmed transaction held (listed by a trusted validator, admitted by a peer
    /// quorum, or ours), in txid order
    pub fn entries(&self) -> impl Iterator<Item = MempoolEntry> + '_ {
        self.0.mempool.iter().map(|(txid, sighting)| sighting.entry(*txid))
    }

    pub fn get(&self, txid: &TxId) -> Option<MempoolEntry> {
        self.0.mempool.get(txid).map(|sighting| sighting.entry(*txid))
    }

    /// `GetMempoolTx`'s filter (`service.proto:293-305`): all but the entries a suffix identifies
    ///
    /// - suffix vs protocol-order txid bytes
    /// - entry excluded only on a unique match (ambiguous → kept)
    pub fn excluding<B: AsRef<[u8]>>(&self, suffixes: &[B]) -> Vec<MempoolEntry> {
        let servable: Vec<MempoolEntry> = self.entries().collect();
        let mut excluded: Vec<TxId> = Vec::new();

        for suffix in suffixes {
            let mut matched =
                servable.iter().filter(|entry| entry.txid.as_ref().ends_with(suffix.as_ref()));
            if let (Some(only), None) = (matched.next(), matched.next()) {
                excluded.push(only.txid);
            }
        }

        servable.into_iter().filter(|entry| !excluded.contains(&entry.txid)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sightings_render_is_shared_by_every_snapshot_until_its_first_listed_fee() {
        let txid = TxId::from_bytes([1; 32]);
        let mut sighting = Sighting::relayed(Bytes::from_static(b"tx"));
        let published = sighting.clone();
        let render = |sighting: &Sighting, text: &'static str| {
            sighting.entry(txid).projection.get_or_render(|| Ok::<_, ()>(Bytes::from(text)))
        };

        let first = render(&sighting, "first");
        let shared = render(&published, "again");
        sighting.listed_fee(Zatoshis::ZERO);
        let priced = render(&sighting, "priced");

        assert_eq!(first, Ok(Bytes::from("first")), "unpriced: rendered once");
        assert_eq!(shared, Ok(Bytes::from("first")), "a published clone: the first render");
        assert_eq!(priced, Ok(Bytes::from("priced")), "a first listed fee: rendered afresh");
    }
}
