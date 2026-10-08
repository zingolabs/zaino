//! Headers-first: every trusted validator's chain into the header chain, its [`VerifiedChain`]
//! into the view and onto a watch (`verified-chain.md` §4)
//!
//! ```text
//!   view changed ─▶ answering validators' claims + getblockhash answers held ─▶ vouch, finalize
//!               ─▶ first validator whose claim we lack (not backing off, not forked below final):
//!                  one batch ≤ ceiling ─▶ stage A (parallel) ─▶ stage B (ordered) ─▶ vouch the
//!                  run's last header (its best chain) ─▶ finalize ─▶ VerifiedChain → view + watch
//! ```
//!
//! - one owner, no lock: the chain moves to the blocking pool for stage B + finality (store
//!   commit = disk) and back; stage A = one blocking task per core
//! - final = min(vouched, best − depth) after every run (H6): a trusted run vouches itself
//! - backpressure: no fetch above `ceiling(HEADER_BATCH)` (tree ≤ depth + batch above final, H9)
//! - per validator: a stall (fetch failed, retreated, invalid, from the future) backs off only it
//!   (`RETRY`); a chain forked below our final tip is not fetched again until its claim moves
//! - a store commit failure ends the task ([`HeaderStoreFailed`]): the supervisor ends the process
//! - an undecodable header or one failing a rule = that validator served an invalid chain:
//!   reported (benched); a header from the future = deferred (never blamed: H7), as is an orphan

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_header_chain::{
    check, decode_header, link_run, Checked, Header, HeaderChain, Params, Rejected, VerifiedChain,
};
use zaino_primitives::types::{BlockRef, Height};
use zaino_source::{ChainDataSource, GetAtHeightError};
use zaino_traffic::{HeaderAsk, TrafficBalancer, ValidatorId};

use crate::error::HeaderStoreFailed;
use crate::fold::ChainViewCore;
use crate::snapshot::ChainViewSnapshot;

/// Heights per fetch + verify step (tree ≤ `depth` + this above the final tip)
pub(crate) const HEADER_BATCH: u32 = 2_000;

/// One validator's back-off after a stalled run
const RETRY: Duration = Duration::from_secs(5);

/// Feeds one header chain from every trusted validator; owns it
pub struct HeaderSync<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    /// `None` only while lent to the blocking pool ([`Self::blocking`])
    chain: Option<HeaderChain>,
    verified: watch::Sender<Option<Arc<VerifiedChain>>>,
    members: Vec<Member>,
}

/// One validator's fetch state
///
/// - `next` = where its next run starts (`None` = above our best, capped at its claim)
/// - `forked` = a claim whose chain leaves ours below the final tip (skipped while it stands)
#[derive(Debug, Clone, Copy, Default)]
struct Member {
    next: Option<Height>,
    forked: Option<BlockRef>,
    retry_at: Option<Instant>,
}

impl Member {
    /// Backs off `RETRY` (only it), then starts afresh
    fn stalled() -> Self {
        Self { retry_at: Some(Instant::now() + RETRY), ..Self::default() }
    }
}

/// One batch to fetch: `member`'s heights `from..=to`, toward its `claim`
#[derive(Debug, Clone, Copy)]
struct Run {
    member: ValidatorId,
    from: Height,
    to: Height,
    claim: BlockRef,
}

impl<S: ChainDataSource> HeaderSync<S> {
    pub(crate) fn new(
        core: Arc<ChainViewCore>,
        balancer: TrafficBalancer<S>,
        chain: HeaderChain,
    ) -> Self {
        let verified = watch::Sender::new(chain.verified().map(Arc::new));
        let members = vec![Member::default(); core.current().endpoints().len()];
        Self { core, balancer, chain: Some(chain), verified, members }
    }

    /// The verified chain, republished whenever its best or final tip moves (`None` = nothing
    /// verified yet)
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<VerifiedChain>>> {
        self.verified.subscribe()
    }

    /// Until `cancel` (then `Ok`): the view's facts vouched, then one run, else a wait for the
    /// view or the earliest back-off
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), HeaderStoreFailed> {
        let mut published = self.core.subscribe_published();
        self.publish();
        loop {
            published.borrow_and_update();
            let step = async {
                let view = self.core.current();
                self.vouch(&view).await?;
                match self.next_run(&view, Instant::now()) {
                    Ok(run) => self.fetch(run).await,
                    Err(retry) => {
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
                        Ok(())
                    }
                }
            };
            match cancel.run_until_cancelled(step).await {
                None => return Ok(()),
                Some(stepped) => stepped?,
            }
        }
    }

    /// Answering validators' claims + `getblockhash` answers we hold: vouched (each once had it
    /// on its best chain), then finality
    async fn vouch(&mut self, view: &ChainViewSnapshot) -> Result<(), HeaderStoreFailed> {
        let verified = self.verified.borrow().clone();
        let Some(verified) = verified else { return Ok(()) };
        let floor = above_final(verified.final_tip());
        let held = |at: &BlockRef| at.height >= floor && verified.holds(*at);
        let answering = view.endpoints().iter().filter(|meta| meta.answering());
        let facts: Vec<BlockRef> = answering
            .flat_map(|meta| meta.tip().into_iter().chain(meta.held))
            .filter(held)
            .collect();
        if facts.is_empty() {
            return Ok(());
        }
        self.blocking(move |chain| {
            facts.iter().for_each(|fact| chain.vouch(*fact));
            finalize(chain)
        })
        .await?;
        self.publish();
        Ok(())
    }

    /// First answering validator whose claim we lack, not backing off, not forked below final;
    /// `Err` = none (the earliest back-off, if any)
    fn next_run(&mut self, view: &ChainViewSnapshot, now: Instant) -> Result<Run, Option<Instant>> {
        let verified = self.verified.borrow().clone();
        let chain = self.chain.as_ref().expect("header chain back from the blocking pool");
        let ceiling = chain.ceiling(HEADER_BATCH);
        let floor = above_final(chain.final_tip());
        let mut retry: Option<Instant> = None;
        for (at, meta) in view.endpoints().iter().enumerate() {
            let (Some(claim), Some(member), true) =
                (meta.tip(), self.members.get_mut(at), meta.answering())
            else {
                continue;
            };
            if verified.as_ref().is_some_and(|verified| verified.holds(claim)) {
                *member = Member::default();
                continue;
            }
            if let Some(at) = member.retry_at.filter(|at| *at > now) {
                retry = Some(retry.map_or(at, |retry| retry.min(at)));
                continue;
            }
            if member.forked == Some(claim) {
                continue;
            }
            let above_best = verified.as_ref().map_or(Height::GENESIS, |v| v.best().height.next());
            let next = member.next.filter(|next| *next <= claim.height);
            let from = next.unwrap_or(above_best.min(claim.height)).max(floor);
            let to = claim.height.min(ceiling);
            let to = from.checked_add(HEADER_BATCH - 1).map_or(to, |last| last.min(to));
            let member = ValidatorId::new(at).expect("configured below ValidatorId::MAX");
            if from <= to {
                return Ok(Run { member, from, to, claim });
            }
        }
        Err(retry)
    }

    /// One batch from `run.member`: decoded, verified, inserted, its last header vouched; its
    /// fetch state follows the outcome
    async fn fetch(&mut self, run: Run) -> Result<(), HeaderStoreFailed> {
        let after = self.fetched(run).await?;
        self.members[run.member.get()] = after;
        Ok(())
    }

    async fn fetched(&mut self, run: Run) -> Result<Member, HeaderStoreFailed> {
        let Run { member, from, to, claim } = run;
        let address = self.core.current().endpoints()[member.get()].address.clone();
        let heights: Vec<Height> = from.up_to(to).collect();
        let answered = match self.balancer.headers(HeaderAsk::Pinned { member, heights }).await {
            Ok(answered) => answered,
            Err(unanswered) => {
                warn!(endpoint = %address, %unanswered, "Header fetch failed");
                return Ok(Member::stalled());
            }
        };
        let mut headers = Vec::with_capacity(answered.value.len());
        for link in answered.value {
            match link.map(|link| decode_header(&link.header)) {
                Ok(Ok(header)) => headers.push(header),
                Ok(Err(malformed)) => {
                    self.balancer.report(answered.ticket, &malformed);
                    return Ok(Member::stalled());
                }
                // retreated since its claim was read: its next claim is read again
                Err(GetAtHeightError::HeightNotFound(_)) => return Ok(Member::stalled()),
            }
        }
        let Some(top) = headers.last().map(|header| BlockRef { hash: header.hash(), height: to })
        else {
            return Ok(Member::stalled());
        };
        let refused = self.insert(headers, top).await?;
        let verified = self.verified.borrow().clone();
        let holds = |at: BlockRef| verified.as_ref().is_some_and(|v| v.holds(at));
        let on_best = verified.as_ref().is_some_and(|v| v.hash_at(to) == Some(top.hash));
        Ok(match refused {
            // its whole chain read, its claim still not held (an evicted side branch)
            None if to == claim.height && !holds(claim) => Member::stalled(),
            // off our best: its branch continued from there
            None => Member { next: (!on_best).then(|| to.next()), ..Member::default() },
            // its chain leaves ours below `from`: from the final tip, then given up on this claim
            Some(Rejected::Orphan | Rejected::BelowFinal) => {
                match from <= above_final(verified.as_ref().and_then(|v| v.final_tip())) {
                    true => Member { forked: Some(claim), ..Member::default() },
                    false => Member { next: Some(Height::GENESIS), ..Member::default() },
                }
            }
            Some(deferred) if deferred.is_deferred() => {
                debug!(endpoint = %address, %deferred, "Header from the future: retried later");
                Member::stalled()
            }
            Some(rejected) => {
                self.balancer.report(answered.ticket, &rejected);
                Member::stalled()
            }
        })
    }

    /// Stage A, then stage B, the run's last header vouched (fetched by height off its best
    /// chain) and finality on the blocking pool, then publish; `Some` = the rule the run broke
    async fn insert(
        &mut self,
        headers: Vec<Header>,
        top: BlockRef,
    ) -> Result<Option<Rejected>, HeaderStoreFailed> {
        let params =
            *self.chain.as_ref().expect("header chain back from the blocking pool").params();
        let (run, cut) = stage_a(params, headers).await;
        let refused = self
            .blocking(move |chain| {
                let refused = stage_b(chain, run, unix_now()).or(cut);
                if refused.is_none() {
                    chain.vouch(top);
                }
                finalize(chain).map(|()| refused)
            })
            .await?;
        self.publish();
        Ok(refused)
    }

    /// `work` on the chain, off the runtime; the chain comes back with the answer
    async fn blocking<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&mut HeaderChain) -> R + Send + 'static,
    ) -> R {
        let mut chain = self.chain.take().expect("header chain back from the blocking pool");
        let (chain, answer) = tokio::task::spawn_blocking(move || {
            let answer = work(&mut chain);
            if cfg!(debug_assertions) {
                chain.check();
            }
            (chain, answer)
        })
        .await
        .expect("header verification never panics");
        self.chain = Some(chain);
        answer
    }

    /// `VerifiedChain` → watch + view when its best or final tip moved
    fn publish(&mut self) {
        let chain = self.chain.as_ref().expect("header chain back from the blocking pool");
        let tips = (chain.best().map(|best| best.block), chain.final_tip());
        let moved = |old: &VerifiedChain| (Some(old.best()), old.final_tip()) != tips;
        let verified = chain.verified().map(Arc::new);
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

/// First height above `final_tip` (genesis while nothing is final)
fn above_final(final_tip: Option<BlockRef>) -> Height {
    final_tip.map_or(Height::GENESIS, |tip| tip.height.next())
}

/// Final → min(vouched, best − depth) in one commit
fn finalize(chain: &mut HeaderChain) -> Result<(), HeaderStoreFailed> {
    let Some(boundary) = chain.finalizable() else { return Ok(()) };
    let before = chain.final_tip().map_or(0, |tip| u32::from(tip.height));
    chain.finalize(boundary)?;
    if u32::from(boundary.height) / 100_000 > before / 100_000 {
        info!(height = u32::from(boundary.height), "Headers verified and final");
    }
    Ok(())
}

/// Each header alone (version, nBits, solution, proof of work), one blocking task per core,
/// then linkage within the run; cut at the first failure
async fn stage_a(params: Params, mut headers: Vec<Header>) -> (Vec<Checked>, Option<Rejected>) {
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let size = headers.len().div_ceil(cores).max(1);
    let mut lanes = Vec::new();
    while !headers.is_empty() {
        let rest = headers.split_off(size.min(headers.len()));
        let chunk = std::mem::replace(&mut headers, rest);
        lanes.push(tokio::task::spawn_blocking(move || {
            let mut checked = Vec::with_capacity(chunk.len());
            for header in chunk {
                let failed = check(&params, header);
                let stop = failed.is_err();
                checked.push(failed);
                if stop {
                    break;
                }
            }
            checked
        }));
    }
    let mut checked = Vec::new();
    for lane in lanes {
        checked.extend(lane.await.expect("stage A never panics"));
    }
    link_run(checked)
}

/// Every header of `run` in order; `Some` = the rule that refused one (the rest unread)
fn stage_b(chain: &mut HeaderChain, run: Vec<Checked>, now: i64) -> Option<Rejected> {
    run.iter().find_map(|header| chain.insert(header, now).err())
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64
}
