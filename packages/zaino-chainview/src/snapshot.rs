//! The published view: quorum tip, sighted transactions, per-endpoint metadata.
//!
//! - `imbl` collections, so republishing on every fold clones in `O(1)` with structural sharing
//! - reader takes **one `ArcSwap` load per request or stream**, pinning a coherent view — a fold
//!   landing mid-stream cannot splice two views into one response
//! - raw bytes only, never a decoded transaction (parsing = the wire adapter's job, and
//!   `zaino-proto` must not reach this crate)

use bytes::Bytes;
use imbl::{OrdMap, Vector};
use zaino_primitives::types::{TransactionId, Zatoshis};

use crate::endpoints::{EndpointIndex, EndpointSet, ValidatorMetadata};
use crate::error::BelowQuorum;
use crate::quorum::{Quorum, QuorumTip};
use crate::telemetry::Alarms;

/// One unconfirmed transaction, as served
///
/// - `fee` = a validator's listing (it resolved the prevouts); `None` = our own broadcast, not
///   yet listed by any validator
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolEntry {
    pub txid: TransactionId,
    pub raw: Bytes,
    pub fee: Option<Zatoshis>,
}

/// One transaction and where it has been seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sighting {
    seen_at: EndpointSet,
    ours: bool,
    raw: Bytes,
    fee: Option<Zatoshis>,
}

impl Sighting {
    pub(crate) fn new(raw: Bytes, fee: Option<Zatoshis>, ours: bool) -> Self {
        Self { seen_at: EndpointSet::default(), ours, raw, fee }
    }

    /// First listed fee kept (a fee = f(tx, its prevouts): every validator lists the same one)
    pub(crate) fn listed_fee(&mut self, fee: Zatoshis) {
        self.fee.get_or_insert(fee);
    }

    fn entry(&self, txid: TransactionId) -> MempoolEntry {
        MempoolEntry { txid, raw: self.raw.clone(), fee: self.fee }
    }

    /// Which endpoints report it.
    pub(crate) fn seen_at(&self) -> EndpointSet {
        self.seen_at
    }

    /// Relayed by us, so known before it propagated anywhere.
    pub(crate) fn ours(&self) -> bool {
        self.ours
    }

    /// Quorum, or ours (§5) — the exception that lets a wallet see its own send.
    pub(crate) fn servable(&self, quorum: &Quorum) -> bool {
        self.ours || quorum.met_by(self.seen_at)
    }

    pub(crate) fn sight(&mut self, endpoint: EndpointIndex) {
        self.seen_at.insert(endpoint);
    }

    pub(crate) fn unsight(&mut self, endpoint: EndpointIndex) {
        self.seen_at.remove(endpoint);
    }

    pub(crate) fn mark_ours(&mut self) {
        self.ours = true;
    }
}

/// One coherent answer from N validators.
///
/// [`tip`](Self::tip) is `None` below quorum — no answer rather than a weak one — and
/// [`mempool`](Self::mempool) refuses on the same condition.
///
/// - `epoch` bumps on every tip *block* change (incl. to/from `None`): `A → B → A` between two
///   reads still reads as moved; an `agreed_by`-only change does not bump it
/// - `arrivals` = txids turned servable this epoch, in order (repeats on a re-admission)
/// - `agreeing` = largest group holding one common block (= `tip.agreed_by` at quorum)
#[derive(Debug, Clone, PartialEq)]
pub struct ChainViewSnapshot {
    pub(crate) tip: Option<QuorumTip>,
    pub(crate) agreeing: EndpointSet,
    epoch: u64,
    /// Ordered, so two readers of one view walk the mempool identically.
    pub(crate) mempool: OrdMap<TransactionId, Sighting>,
    arrivals: Vector<TransactionId>,
    pub(crate) endpoints: Vector<ValidatorMetadata>,
    pub(crate) alarms: Alarms,
    quorum: Quorum,
}

impl ChainViewSnapshot {
    pub(crate) fn empty(endpoints: Vector<ValidatorMetadata>, quorum: Quorum) -> Self {
        Self {
            tip: None,
            agreeing: EndpointSet::default(),
            epoch: 0,
            mempool: OrdMap::new(),
            arrivals: Vector::new(),
            endpoints,
            alarms: Alarms::default(),
            quorum,
        }
    }

    /// New epoch: arrivals restart (the tails of the old one close on it)
    pub(crate) fn tip_moved(&mut self) {
        self.epoch += 1;
        self.arrivals = Vector::new();
    }

    pub(crate) fn arrived(&mut self, txid: TransactionId) {
        self.arrivals.push_back(txid);
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn arrivals(&self) -> &Vector<TransactionId> {
        &self.arrivals
    }

    /// Highest block ≥threshold voters' chains hold. `None` below quorum.
    pub(crate) fn tip(&self) -> Option<QuorumTip> {
        self.tip
    }

    /// Partition / eclipse / stale-tip conditions as of the last fold (telemetry, never a vote)
    pub fn alarms(&self) -> Alarms {
        self.alarms
    }

    /// Per-endpoint metadata, in configured order
    pub fn endpoints(&self) -> &Vector<ValidatorMetadata> {
        &self.endpoints
    }

    /// Where one transaction has been seen, regardless of whether it is servable.
    ///
    /// Telemetry and the action stream read this; the mempool RPCs go through
    /// [`mempool`](Self::mempool).
    pub(crate) fn sighting(&self, txid: &TransactionId) -> Option<&Sighting> {
        self.mempool.get(txid)
    }

    /// Every transaction the view knows of, servable or not, in txid order.
    pub(crate) fn sightings(&self) -> impl Iterator<Item = (&TransactionId, &Sighting)> + '_ {
        self.mempool.iter()
    }

    /// The mempool, or the refusal that stands in for it below quorum.
    ///
    /// A `Result` rather than an empty answer: below quorum there is no honest answer to give,
    /// and a caller must not be able to forget that (§4, fail closed).
    pub fn mempool(&self) -> Result<MempoolView<'_>, BelowQuorum> {
        match self.tip {
            Some(_) => Ok(MempoolView(self)),
            None => Err(self.quorum.shortfall(self.agreeing)),
        }
    }
}

/// The servable mempool of a snapshot that has quorum.
///
/// Every method here applies the per-transaction rule — `seen_at.count() >= threshold || ours`
/// — so nothing below it can leak out.
#[derive(Debug, Clone, Copy)]
pub struct MempoolView<'a>(&'a ChainViewSnapshot);

impl<'a> MempoolView<'a> {
    /// Callers checked `snapshot.mempool()` already (a tail's anchor)
    pub(crate) fn of(snapshot: &'a ChainViewSnapshot) -> Self {
        Self(snapshot)
    }

    fn servable(&self, sighting: &Sighting) -> bool {
        sighting.servable(&self.0.quorum)
    }

    /// One servable unconfirmed transaction.
    pub(crate) fn get(&self, txid: &TransactionId) -> Option<MempoolEntry> {
        self.0
            .mempool
            .get(txid)
            .filter(|sighting| self.servable(sighting))
            .map(|sighting| sighting.entry(*txid))
    }

    pub(crate) fn serves(&self, txid: &TransactionId) -> bool {
        self.0.mempool.get(txid).is_some_and(|sighting| self.servable(sighting))
    }

    /// Every servable unconfirmed transaction, in txid order.
    pub fn entries(&self) -> impl Iterator<Item = MempoolEntry> + '_ {
        self.0
            .mempool
            .iter()
            .filter(|(_, sighting)| self.servable(sighting))
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
