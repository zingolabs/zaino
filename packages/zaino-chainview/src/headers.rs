//! Trusted validators' and peers' headers into the header chain, its [`HeaderChainSnapshot`] into the
//! view and onto a watch (`docs/architecture.md` §4)
//!
//! ```text
//!   view changed ─▶ answering validators' claims + getblockhash answers held ─▶ vouch, finalize
//!               ─▶ first validator whose claim we lack (no back-off, not forked below finalized):
//!                  no chain, or its claim − below past our ceiling ─▶ anchor at claim − below
//!                  (below = max(depth, CONTEXT): `HeaderChain::anchor_below`)
//!                  else one batch ≤ ceiling ─▶ insert in order (link + work: trusted, no rule
//!                  run) ─▶ vouch the run's last header ─▶ finalize ─▶ HeaderChainSnapshot → view + watch
//! ```
//!
//! - one owner, no lock, no pool: a trusted insert = a hash lookup + nBits work
//! - anchored below a trusted validator's claim, never synced from genesis (zebra validated
//!   every block below it, bulk sync reads blocks by height, not this chain)
//! - finalized = min(vouched, best − depth) after every run (H6): a trusted run vouches itself
//! - backpressure: no fetch above `ceiling(HEADER_BATCH)` (tree ≤ depth + batch above finalized,
//!   H9)
//! - per validator: a stall (fetch failed, retreated, malformed) backs off only it (`RETRY`)
//! - chain forked below our finalized tip → not fetched again until its claim moves
//! - peers ([`HeaderSync::with_peers`]): no trusted run due → peer set asked past our best
//!   (locator) every `PEER_POLL`, at once again after progress
//! - peer headers: every rule run per header (untrusted), never vouched
//! - peer finality: no vouch that deep → best − depth finalized anyway once live peers in more than
//!   `ECLIPSE_NETGROUPS` netgroups announced a block at or above it and no difficulty
//!   collapsed since the last vouch (`HeaderChain::peer_finalizable`)

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_header_chain::{
    decode_header, Header, HeaderChain, HeaderChainSnapshot, Inserted, Params, Rejected,
};
use zaino_primitives::types::BlockRef;
use zaino_source::{BlockLinks, ChainDataSource, GetAtHeightError};
use zaino_traffic::{Answered, HeaderAsk, TrafficBalancer, TrustedValidatorId};
use zcash_protocol::consensus::{BlockHeight, H0};

use crate::chain_view_snapshot::ChainViewSnapshot;
use crate::merge::ChainViewCore;
use crate::netgroup::{self, ECLIPSE_NETGROUPS};

/// Heights per fetch step (tree ≤ `depth` + this above the finalized tip)
pub(crate) const HEADER_BATCH: u32 = 2_000;

/// One validator's back-off after a stalled run
const RETRY: Duration = Duration::from_secs(5);

/// Peer set asked past our best this often (nothing newer = an empty answer, a block ≈ 75 s)
const PEER_POLL: Duration = Duration::from_secs(5);

/// Feeds + owns one header chain from every trusted validator (and the peer set, if any)
pub struct HeaderSync<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    chain: HeaderChain,
    verified: watch::Sender<Option<Arc<HeaderChainSnapshot>>>,
    fetch_states: Vec<FetchState>,
    peers: Option<Peers>,
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
        let verified = watch::Sender::new(chain.snapshot().map(Arc::new));
        let fetch_states = vec![FetchState::default(); core.current().trusted_validators().len()];
        Self { core, balancer, chain, verified, fetch_states, peers: None }
    }

    /// Headers from the peer set too, each held to `params` (the balancer's peer-set member)
    pub fn with_peers(self, params: Params) -> Self {
        Self { peers: Some(Peers { params, due: Instant::now() }), ..self }
    }

    /// The verified chain, republished whenever its best or finalized tip moves (`None` = no anchor
    /// yet)
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<HeaderChainSnapshot>>> {
        self.verified.subscribe()
    }

    /// Until `cancel`: the view's facts vouched, then one run, else a wait for the view or the
    /// earliest back-off
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut published = self.core.subscribe_published();
        self.publish();
        loop {
            published.borrow_and_update();
            let step = async {
                let view = self.core.current();
                self.vouch(&view);
                let now = Instant::now();
                match self.next_run(&view, now) {
                    Ok(run) => self.fetch(run).await,
                    Err(_) if self.peers_due(now) => self.extend_from_peers().await,
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

    /// Answering validators' claims + `getblockhash` answers we hold: vouched (each once had it
    /// on its best chain), then finality (every view change: a peer's block `inv` may be what
    /// peer finality waited on)
    fn vouch(&mut self, view: &ChainViewSnapshot) {
        let verified = self.verified.borrow().clone();
        let Some(verified) = verified else { return };
        let floor = verified.finalized_tip().height + 1;
        let held = |at: &BlockRef| at.height >= floor && verified.holds(*at);
        let answering = view.trusted_validators().iter().filter(|meta| meta.answering());
        let facts = answering.flat_map(|meta| meta.tip().into_iter().chain(meta.block_at_best));
        facts.filter(held).for_each(|fact| self.chain.vouch(fact));
        self.finalize(view);
        self.publish();
    }

    /// First answering validator whose claim we lack, not backing off, not forked below finalized
    ///
    /// - `Err` = none (the earliest back-off, if any)
    fn next_run(&mut self, view: &ChainViewSnapshot, now: Instant) -> Result<Run, Option<Instant>> {
        let verified = self.verified.borrow().clone();
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
            if verified.as_ref().is_some_and(|verified| verified.holds(claim)) {
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
            let above_best = verified.as_ref().map_or(H0, |v| v.best().height + 1);
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
            self.chain.vouch(top);
        }
        self.after_inserts();
        let verified = self.verified.borrow().clone();
        let holds = |at: BlockRef| verified.as_ref().is_some_and(|v| v.holds(at));
        let on_best = verified.as_ref().is_some_and(|v| v.hash_at(to) == Some(top.hash));
        let floor = verified.as_ref().map_or(H0, |v| v.finalized_tip().height + 1);
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

    /// Peer path on and due, the chain anchored (a locator to ask from)
    fn peers_due(&self, now: Instant) -> bool {
        let anchored = self.chain.finalized_tip().is_some();
        anchored && self.peers.as_ref().is_some_and(|peers| peers.due <= now)
    }

    /// One peer-set answer past our best, inserted under every rule
    ///
    /// - asked again at once after progress, else after `PEER_POLL`
    async fn extend_from_peers(&mut self) {
        let verified = self.verified.borrow().clone();
        let locator = verified.map(|verified| verified.locator()).unwrap_or_default();
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
                // not the peer's fault: a clock behind, a fork we cannot place, too little held
                Err(refused)
                    if refused.is_deferred()
                        || matches!(
                            refused,
                            Rejected::Orphan
                                | Rejected::BelowFinalized
                                | Rejected::ShortContext { .. }
                        ) =>
                {
                    break
                }
                Err(refused) => {
                    self.balancer.report(answered.ticket, &refused);
                    break;
                }
            }
        }
        if progressed {
            self.after_inserts();
        }
        progressed
    }

    /// Finality against the current view, then publish
    fn after_inserts(&mut self) {
        self.finalize(&self.core.current());
        self.publish();
    }

    /// - finalized → min(vouched, best − depth)
    /// - peers on + no vouch that deep → best − depth on the peers' word (no difficulty collapse,
    ///   enough netgroups carrying it)
    fn finalize(&mut self, view: &ChainViewSnapshot) {
        if let Some(boundary) = self.chain.finalizable() {
            self.chain.finalize(boundary);
        }
        if self.peers.is_none() {
            return;
        }
        let Some(boundary) = self.chain.peer_finalizable() else { return };
        let netgroups = self.netgroups_carrying(view, boundary.height);
        if netgroups > ECLIPSE_NETGROUPS {
            let height = u32::from(boundary.height);
            debug!(height, hash = %boundary.hash, netgroups, "Finalized on peers' word, unvouched");
            self.chain.finalize_by_peers(boundary);
        }
    }

    /// Netgroups of live peers whose latest announced block is on our best chain at or above
    /// `from` (its chain holds `from`)
    fn netgroups_carrying(&self, view: &ChainViewSnapshot, from: BlockHeight) -> usize {
        let carrying = view.peers_carrying.iter().filter(|(_, announced)| {
            self.chain.best_height_of(**announced).is_some_and(|height| height >= from)
        });
        netgroup::count(carrying.map(|(peer, _)| *peer))
    }

    /// `HeaderChainSnapshot` → watch + view when its best or finalized tip moved
    fn publish(&mut self) {
        let tips = (self.chain.best().map(|best| best.block), self.chain.finalized_tip());
        let moved =
            |old: &HeaderChainSnapshot| (Some(old.best()), Some(old.finalized_tip())) != tips;
        let verified = self.chain.snapshot().map(Arc::new);
        let changed = self.verified.send_if_modified(|published| {
            let changed = published.as_deref().is_none_or(moved) && verified.is_some();
            if changed {
                *published = verified;
            }
            changed
        });
        if changed {
            self.core.apply_headers(self.verified.borrow().clone());
        }
    }
}
