//! Trusted validators' and peers' headers into the header chain, its [`HeaderChainSnapshot`] into
//! the view and onto a watch (`docs/architecture.md` §4)
//!
//! ```text
//!   view changed ─▶ answering validators' claims + getblockhash answers, live peers' invs
//!                   ─▶ witnessed, finalize
//!               ─▶ first validator whose claim we lack (no back-off, not forked below finalized):
//!                  no chain, or its claim − below past our ceiling ─▶ anchor at claim − below
//!                  (below = max(depth, CONTEXT): `HeaderChain::anchor_below`)
//!                  else one batch ≤ ceiling ─▶ insert in order (link + work: trusted, no rule
//!                  run) ─▶ vouch the run's last header ─▶ finalize ─▶ snapshot → view + watch
//! ```
//!
//! - one owner, no lock, no pool: a trusted insert = a hash lookup + nBits work
//! - anchored below a trusted validator's claim, never synced from genesis (zebra validated
//!   every block below it, bulk sync reads blocks by height, not this chain)
//! - finalized = min(settled, best − depth) after every change (H6: `HeaderChain::finalizable`)
//! - backpressure: no fetch above `ceiling(HEADER_BATCH)` (tree ≤ depth + batch above finalized,
//!   H9)
//! - per validator: a stall (fetch failed, retreated, malformed) backs off only it (`RETRY`)
//! - chain forked below our finalized tip → not fetched again until its claim moves
//! - peers ([`HeaderSync::with_peers`]): no trusted run due → peer set asked past our best
//!   (locator) every `PEER_POLL`, at once again after progress
//! - peer headers: every rule run per header (untrusted), never vouched

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_header_chain::{
    decode_header, Fault, Header, HeaderChain, HeaderChainSnapshot, Inserted, Params, Rejected,
    Witness,
};
use zaino_primitives::types::{BlockRef, Netgroup};
use zaino_source::{BlockLink, BlockLinks, ChainDataSource, GetAtHeightError};
use zaino_traffic::{Answered, HeaderAsk, TrafficBalancer, TrustedValidatorId};
use zcash_protocol::consensus::{BlockHeight, H0};

use crate::chain_view_core::ChainViewCore;
use crate::chain_view_snapshot::ChainViewSnapshot;

/// Heights per fetch step (tree ≤ `depth` + this above the finalized tip)
pub(crate) const HEADER_BATCH: u32 = 2_000;

/// One validator's back-off after a stalled run
const RETRY: Duration = Duration::from_secs(5);

/// Peer set asked past our best this often (nothing newer = an empty answer, a block ≈ 75 s)
pub(crate) const PEER_POLL: Duration = Duration::from_secs(5);

/// Feeds + owns one header chain from every trusted validator (and the peer set, if any)
///
/// - `published` = the chain's revision last sent
/// - `own` = our indexes' chain by hash ([`with_own_chain`](Self::with_own_chain)), until anchored
pub struct HeaderSync<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    chain: HeaderChain,
    snapshots: watch::Sender<Option<Arc<HeaderChainSnapshot>>>,
    published: Option<u64>,
    fetch_states: Vec<FetchState>,
    peers: Option<Peers>,
    own: Option<Vec<BlockRef>>,
}

/// The peer path: the rules its headers meet, when it is next asked
struct Peers {
    params: Params,
    due: Instant,
}

/// One validator's fetch state
///
/// - `next` = where its next run starts (`None` = above our best, capped at its claim)
/// - `forked` = a claim whose chain leaves ours below the finalized tip (skipped while it stands)
#[derive(Debug, Clone, Copy, Default)]
struct FetchState {
    next: Option<BlockHeight>,
    forked: Option<BlockRef>,
    retry_at: Option<Instant>,
}

impl FetchState {
    /// Backs off `RETRY` (only it), then starts afresh
    fn stalled() -> Self {
        Self { retry_at: Some(Instant::now() + RETRY), ..Self::default() }
    }
}

/// - `Anchor` = the header at `at` becomes the finalized tip (no chain, or `validator` far ahead)
/// - `Extend` = `validator`'s heights `from..=to`, toward its `claim`
#[derive(Debug, Clone, Copy)]
enum Run {
    Anchor { validator: TrustedValidatorId, at: BlockHeight },
    Extend { validator: TrustedValidatorId, from: BlockHeight, to: BlockHeight, claim: BlockRef },
}

impl<S: ChainDataSource> HeaderSync<S> {
    pub(crate) fn new(
        core: Arc<ChainViewCore>,
        balancer: TrafficBalancer<S>,
        chain: HeaderChain,
    ) -> Self {
        let snapshots = watch::Sender::new(chain.snapshot().map(Arc::new));
        let fetch_states = vec![FetchState::default(); core.current().trusted_validators().len()];
        let (published, peers, own) = (None, None, None);
        Self { core, balancer, chain, snapshots, published, fetch_states, peers, own }
    }

    /// Headers from the peer set too, each held to `params` (the balancer's peer-set member)
    pub fn with_peers(self, params: Params) -> Self {
        Self { peers: Some(Peers { params, due: Instant::now() }), ..self }
    }

    /// Anchor with no trusted validator answering (a restart mid-outage) on our indexes' chain:
    /// `own` = `[locator, anchor, …, durable tip]` by hash (`CONTEXT` + 2 blocks), peers' headers
    /// each matched to our hash in turn
    pub fn with_own_chain(self, own: Vec<BlockRef>) -> Self {
        Self { own: Some(own), ..self }
    }

    /// The header chain, republished on every change (`None` = no anchor yet)
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<HeaderChainSnapshot>>> {
        self.snapshots.subscribe()
    }

    /// Until `cancel`: the view's facts witnessed, then one run, else a wait for the view or the
    /// earliest back-off
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut published = self.core.subscribe_published();
        self.publish();
        loop {
            published.borrow_and_update();
            let step = async {
                let view = self.core.current();
                self.witness(&view);
                let now = Instant::now();
                match self.next_run(&view, now) {
                    Ok(run) => self.fetch(run).await,
                    Err(_) if self.peers_due(now) => match self.chain.finalized_tip() {
                        Some(_) => self.extend_from_peers().await,
                        None => self.anchor_own().await,
                    },
                    Err(retry) => {
                        let peers = self.peers.as_ref().map(|peers| peers.due);
                        let retry = retry.into_iter().chain(peers).min();
                        let backoff = async {
                            match retry {
                                Some(at) => tokio::time::sleep_until(at).await,
                                None => std::future::pending().await,
                            }
                        };
                        tokio::select! {
                            _ = published.changed() => {}
                            () = backoff => {}
                        }
                    }
                }
            };
            if cancel.run_until_cancelled(step).await.is_none() {
                return;
            }
        }
    }

    /// Answering validators' claims + `getblockhash` answers, live peers' latest `inv`s: each
    /// source once had that block on its best chain
    fn witness(&mut self, view: &ChainViewSnapshot) {
        let answering = view.trusted_validators().iter().filter(|meta| meta.answering());
        for fact in answering.flat_map(|meta| meta.tip().into_iter().chain(meta.block_at_best)) {
            self.chain.witness(fact.hash, Witness::Trusted);
        }
        for (peer, hash) in &view.peers_carrying {
            self.chain.witness(*hash, Witness::Peer(Netgroup::of(*peer)));
        }
        self.after_inserts();
    }

    /// First answering validator whose claim we lack, not backing off, not forked below finalized
    ///
    /// - `Err` = none (the earliest back-off, if any)
    fn next_run(&mut self, view: &ChainViewSnapshot, now: Instant) -> Result<Run, Option<Instant>> {
        let snapshot = self.snapshots.borrow().clone();
        let chain = &self.chain;
        let ceiling = chain.ceiling(HEADER_BATCH);
        let floor = chain.finalized_tip().map_or(H0, |tip| tip.height + 1);
        let mut retry: Option<Instant> = None;
        for (at, meta) in view.trusted_validators().iter().enumerate() {
            let (Some(claim), Some(fetch), true) =
                (meta.tip(), self.fetch_states.get_mut(at), meta.answering())
            else {
                continue;
            };
            if snapshot.as_ref().is_some_and(|snapshot| snapshot.holds(claim)) {
                *fetch = FetchState::default();
                continue;
            }
            if let Some(at) = fetch.retry_at.filter(|at| *at > now) {
                retry = Some(retry.map_or(at, |retry| retry.min(at)));
                continue;
            }
            if fetch.forked == Some(claim) {
                continue;
            }
            let id = TrustedValidatorId::new(at).expect("configured below TrustedValidatorId::MAX");
            let anchor = claim.height.saturating_sub(chain.anchor_below());
            if chain.finalized_tip().is_none() || anchor > ceiling {
                return Ok(Run::Anchor { validator: id, at: anchor });
            }
            let above_best = snapshot.as_ref().map_or(H0, |s| s.best().height + 1);
            let next = fetch.next.filter(|next| *next <= claim.height);
            let from = next.unwrap_or(above_best.min(claim.height)).max(floor);
            let to = claim.height.min(ceiling);
            let to = (from + (HEADER_BATCH - 1)).min(to);
            if from <= to {
                return Ok(Run::Extend { validator: id, from, to, claim });
            }
        }
        Err(retry)
    }

    /// One run from its validator (its fetch state follows the outcome)
    async fn fetch(&mut self, run: Run) {
        let (validator, after) = match run {
            Run::Anchor { validator, at } => (validator, self.anchored(validator, at).await),
            Run::Extend { validator, from, to, claim } => {
                (validator, self.extended(validator, from, to, claim).await)
            }
        };
        self.fetch_states[validator.get()] = after;
    }

    /// `validator`'s headers at `heights`, decoded (`Err` = its fetch state: stalled, reported)
    async fn headers(
        &self,
        validator: TrustedValidatorId,
        heights: Vec<BlockHeight>,
    ) -> Result<Vec<Header>, FetchState> {
        let address = self.core.current().trusted_validators()[validator.get()].address.clone();
        let answered = match self.balancer.headers(HeaderAsk::Pinned { validator, heights }).await {
            Ok(answered) => answered,
            Err(unanswered) => {
                warn!(validator = %address, %unanswered, "Header fetch failed");
                return Err(FetchState::stalled());
            }
        };
        let mut headers = Vec::with_capacity(answered.value.len());
        for link in answered.value {
            match link.map(|link| decode_header(&link.header)) {
                Ok(Ok(header)) => headers.push(header),
                Ok(Err(malformed)) => {
                    self.balancer.report(answered.ticket, &malformed);
                    return Err(FetchState::stalled());
                }
                // retreated since its claim was read: its next claim is read again
                Err(GetAtHeightError::HeightNotFound(_)) => return Err(FetchState::stalled()),
            }
        }
        match headers.is_empty() {
            true => Err(FetchState::stalled()),
            false => Ok(headers),
        }
    }

    /// `validator`'s header at `at` = the finalized tip, everything held before dropped
    async fn anchored(&mut self, validator: TrustedValidatorId, at: BlockHeight) -> FetchState {
        let header = match self.headers(validator, vec![at]).await {
            Ok(mut headers) => headers.remove(0),
            Err(after) => return after,
        };
        self.chain.anchor(&header, at);
        info!(height = u32::from(at), hash = %header.hash(), "Header chain anchored");
        self.publish();
        FetchState::default()
    }

    /// `validator`'s heights `from..=to` inserted in order, the last vouched, then finality
    async fn extended(
        &mut self,
        validator: TrustedValidatorId,
        from: BlockHeight,
        to: BlockHeight,
        claim: BlockRef,
    ) -> FetchState {
        let headers = match self
            .headers(validator, (u32::from(from)..=u32::from(to)).map(BlockHeight::from).collect())
            .await
        {
            Ok(headers) => headers,
            Err(after) => return after,
        };
        let top =
            BlockRef { hash: headers.last().expect("headers() Ok = non-empty").hash(), height: to };
        let refused = headers.iter().find_map(|header| self.chain.insert(header).err());
        if refused.is_none() {
            self.chain.witness(top.hash, Witness::Trusted);
        }
        self.after_inserts();
        let snapshot = self.snapshots.borrow().clone();
        let holds = |at: BlockRef| snapshot.as_ref().is_some_and(|s| s.holds(at));
        let on_best = snapshot.as_ref().is_some_and(|s| s.hash_at(to) == Some(top.hash));
        let floor = snapshot.as_ref().map_or(H0, |s| s.finalized_tip().height + 1);
        match refused {
            // its whole chain read, its claim still not held (an evicted side branch)
            None if to == claim.height && !holds(claim) => FetchState::stalled(),
            // off our best: its branch continued from there
            None => FetchState { next: (!on_best).then(|| to + 1), ..FetchState::default() },
            // its chain leaves ours below `from`: from the finalized tip, then given up on this
            // claim
            Some(Rejected::Orphan | Rejected::BelowFinalized) => match from <= floor {
                true => FetchState { forked: Some(claim), ..FetchState::default() },
                false => FetchState { next: Some(floor), ..FetchState::default() },
            },
            Some(rejected) => {
                let address =
                    self.core.current().trusted_validators()[validator.get()].address.clone();
                debug!(validator = %address, %rejected, "Trusted header refused");
                FetchState::stalled()
            }
        }
    }

    /// Peer path on and due, a locator to ask from (the chain anchored, or our own chain)
    fn peers_due(&self, now: Instant) -> bool {
        let locator = self.chain.finalized_tip().is_some() || self.own.is_some();
        locator && self.peers.as_ref().is_some_and(|peers| peers.due <= now)
    }

    /// Peers' headers after `own[0]`, each the hash of `own[1..]` in turn → anchored at `own[1]`,
    /// the rest inserted as trusted (our indexes folded them); else asked again after `PEER_POLL`
    async fn anchor_own(&mut self) {
        let Some(own) = self.own.clone() else { return };
        if let Some(peers) = &mut self.peers {
            peers.due = Instant::now() + PEER_POLL;
        }
        let asked = self.balancer.headers(HeaderAsk::Peers { locator: vec![own[0].hash] }).await;
        let links = asked.map(|answered| answered.value).unwrap_or_default();
        let ours = |(link, at): (Result<BlockLink, _>, &BlockRef)| {
            let header = decode_header(&link.ok()?.header).ok()?;
            (header.hash() == at.hash).then_some(header)
        };
        let headers: Vec<Header> = links.into_iter().zip(&own[1..]).map_while(ours).collect();
        let (Some(anchor), true) = (headers.first(), headers.len() == own.len() - 1) else {
            debug!(matched = headers.len(), "Peers short of our own chain, asked again");
            return;
        };
        self.chain.anchor(anchor, own[1].height);
        for header in &headers[1..] {
            self.chain.insert(header).expect("our own chain: each a child of the last");
        }
        info!(height = u32::from(own[1].height), hash = %own[1].hash, "Header chain anchored on our own indexes");
        self.own = None;
        self.publish();
    }

    /// One peer-set answer past our best, inserted under every rule
    ///
    /// - asked again at once after progress, else after `PEER_POLL`
    async fn extend_from_peers(&mut self) {
        let snapshot = self.snapshots.borrow().clone();
        let locator = snapshot.map(|snapshot| snapshot.locator()).unwrap_or_default();
        let asked = self.balancer.headers(HeaderAsk::Peers { locator }).await;
        let progressed = match asked {
            Ok(answered) => self.insert_from_peers(answered),
            Err(unanswered) => {
                debug!(%unanswered, "Peer header fetch failed");
                false
            }
        };
        let wait = if progressed { Duration::ZERO } else { PEER_POLL };
        if let Some(peers) = &mut self.peers {
            peers.due = Instant::now() + wait;
        }
    }

    /// In order, each under every rule (stops at the first refused or above the ceiling)
    ///
    /// - `true` = one became new
    fn insert_from_peers(&mut self, answered: Answered<BlockLinks>) -> bool {
        let params = self.peers.as_ref().expect("the peer path is on").params;
        let ceiling = self.chain.ceiling(HEADER_BATCH);
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let now_unix = i64::try_from(now_unix).unwrap_or(i64::MAX);
        let mut progressed = false;
        for link in answered.value {
            let header = match link.map(|link| decode_header(&link.header)) {
                Ok(Ok(header)) => header,
                Ok(Err(malformed)) => {
                    self.balancer.report(answered.ticket, &malformed);
                    break;
                }
                Err(_) => break,
            };
            if self.chain.height_of(&header).is_some_and(|at| at > ceiling) {
                break;
            }
            match self.chain.insert_untrusted(&header, &params, now_unix) {
                Ok(Inserted::Known) => {}
                Ok(Inserted::Side | Inserted::Best { .. }) => progressed = true,
                Err(refused) => {
                    if refused.fault() == Fault::Sender {
                        self.balancer.report(answered.ticket, &refused);
                    }
                    break;
                }
            }
        }
        if progressed {
            self.after_inserts();
        }
        progressed
    }

    /// Finality, then publish
    fn after_inserts(&mut self) {
        if let Some(boundary) = self.chain.finalizable() {
            self.chain.finalize(boundary);
        }
        self.publish();
    }

    /// Snapshot → watch + view once anchored, on every change since the last publish
    fn publish(&mut self) {
        let revision = Some(self.chain.revision());
        if self.published == revision {
            return;
        }
        let Some(snapshot) = self.chain.snapshot().map(Arc::new) else { return };
        self.published = revision;
        self.snapshots.send_replace(Some(Arc::clone(&snapshot)));
        self.core.apply_headers(Some(snapshot));
    }
}
