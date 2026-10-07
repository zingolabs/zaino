//! Headers-first: every trusted validator's chain into the header chain, its [`VerifiedChain`]
//! into the view and onto a watch (`verified-chain.md` §4)
//!
//! ```text
//!   view changed ─▶ each answering validator whose claim is off our best chain:
//!                     first height = above its reach (highest verified block it holds), else
//!                     above the final tip
//!                     ─▶ batch fetch ─▶ stage A (each header alone, parallel) ─▶ stage B (ordered)
//!                     ─▶ VerifiedChain → view + watch (+ the run's last header: it holds that)
//!                     ─▶ finalize what is `depth` deep and held (`Holders`)
//! ```
//!
//! - one owner, no lock: the chain moves to the blocking pool for stage B + finality (store
//!   commit = disk) and back; stage A = one blocking task per core
//! - every chain change reaches the view before finality reads its holders (one chain, both sides)
//! - a store commit failure ends the task ([`HeaderStoreFailed`]): the supervisor ends the process
//! - an undecodable header or one failing a rule = that validator served an invalid chain:
//!   reported (benched), skipped this round; a header from the future = deferred (retried after
//!   `RETRY`, never blamed: H7); an orphan run = never blamed either

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_header_chain::{
    check, decode_header, link_run, Checked, Header, HeaderChain, Params, Rejected, VerifiedChain,
};
use zaino_primitives::types::{BlockRef, Height};
use zaino_source::{ChainDataSource, GetAtHeightError};
use zaino_traffic::{HeaderAsk, TrafficBalancer, ValidatorId};

use crate::error::HeaderStoreFailed;
use crate::fold::{ChainViewCore, HeaderReport, Served};
use crate::holders::{Holders, PollStamp};

/// Heights per fetch + verify step (memory: tree holds <= `depth` + this before finalizing)
const HEADER_BATCH: u32 = 2_000;

/// Pause after a round that could not reach every validator's tip
const RETRY: Duration = Duration::from_secs(5);

/// Feeds one header chain from every trusted validator; owns it
///
/// - `reported` = (best, final tip, finality paused) the view last heard
pub struct HeaderSync<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    /// `None` only while lent to the blocking pool ([`Self::blocking`])
    chain: Option<HeaderChain>,
    verified: watch::Sender<Option<Arc<VerifiedChain>>>,
    reported: Option<(Option<BlockRef>, Option<BlockRef>, bool)>,
    finality_paused: bool,
}

/// One round against one validator
enum Synced {
    /// Its claim is on our best chain now
    Caught,
    /// Fetch failed, it retreated since its poll, or it served a header that failed a rule
    Stalled,
}

impl<S: ChainDataSource> HeaderSync<S> {
    pub(crate) fn new(
        core: Arc<ChainViewCore>,
        balancer: TrafficBalancer<S>,
        chain: HeaderChain,
    ) -> Self {
        let verified = watch::Sender::new(chain.verified().map(Arc::new));
        let chain = Some(chain);
        Self { core, balancer, chain, verified, reported: None, finality_paused: false }
    }

    /// The verified chain, republished whenever its best or final tip moves (`None` = nothing
    /// verified yet)
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<VerifiedChain>>> {
        self.verified.subscribe()
    }

    /// Until `cancel` (then `Ok`): a round per view change; a stalled round retried after `RETRY`
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), HeaderStoreFailed> {
        let mut published = self.core.subscribe_published();
        self.publish(None);
        loop {
            let Some(stalled) = cancel.run_until_cancelled(self.round()).await else {
                return Ok(());
            };
            let wait = async {
                match stalled? {
                    true => tokio::time::sleep(RETRY).await,
                    false => {
                        let _ = published.changed().await;
                    }
                }
                Ok::<_, HeaderStoreFailed>(())
            };
            match cancel.run_until_cancelled(wait).await {
                None => return Ok(()),
                Some(waited) => waited?,
            }
        }
    }

    /// `true` = some validator stalled
    async fn round(&mut self) -> Result<bool, HeaderStoreFailed> {
        let mut stalled = false;
        let configured = self.core.current().endpoints().len();
        for index in (0..configured).filter_map(ValidatorId::new) {
            // re-read per validator: the last one's headers may have moved the chain
            let view = self.core.current();
            let Some((claim, under)) = view.holders.claim(index) else { continue };
            let reach = view.holders.reach(index);
            let address = view.endpoints()[index.get()].address.clone();
            if let Synced::Stalled = self.sync_from(index, &address, claim, under, reach).await? {
                stalled = true;
            }
        }
        self.finalize().await?;
        Ok(stalled)
    }

    /// Fetch + insert its chain from above where it leaves ours up to its claim (read `under`
    /// that poll of it)
    async fn sync_from(
        &mut self,
        index: ValidatorId,
        address: &str,
        claim: BlockRef,
        under: PollStamp,
        reach: Option<Height>,
    ) -> Result<Synced, HeaderStoreFailed> {
        let Some(mut next) = self.first_needed(claim, reach) else { return Ok(Synced::Caught) };
        let mut retried_from_final = false;
        while next <= claim.height {
            let last =
                next.checked_add(HEADER_BATCH - 1).map_or(claim.height, |h| h.min(claim.height));
            let heights: Vec<Height> = next.up_to(last).collect();
            let ask = HeaderAsk::Pinned { member: index, heights };
            let answered = match self.balancer.headers(ask).await {
                Ok(answered) => answered,
                Err(unanswered) => {
                    warn!(endpoint = %address, %unanswered, "Header fetch failed");
                    return Ok(Synced::Stalled);
                }
            };
            let mut headers = Vec::with_capacity(answered.value.len());
            for link in answered.value {
                match link {
                    Ok(link) => match decode_header(&link.header) {
                        Ok(header) => headers.push(header),
                        Err(malformed) => {
                            self.balancer.report(answered.ticket, &malformed);
                            return Ok(Synced::Stalled);
                        }
                    },
                    // retreated since its claim was read: next round reads its new one
                    Err(GetAtHeightError::HeightNotFound(_)) => return Ok(Synced::Stalled),
                }
            }
            let Some(top) =
                headers.last().map(|header| BlockRef { hash: header.hash(), height: last })
            else {
                return Ok(Synced::Stalled);
            };
            match self.insert(headers, (index, top, under)).await? {
                None => next = last.next(),
                // its chain leaves ours below where we started: from the final tip, once
                Some(Rejected::Orphan) if !retried_from_final => {
                    retried_from_final = true;
                    next = above_final(self.chain().final_tip());
                }
                Some(Rejected::Orphan) => return Ok(Synced::Stalled),
                Some(deferred) if deferred.is_deferred() => {
                    debug!(endpoint = %address, %deferred, "Header from the future: retried later");
                    return Ok(Synced::Stalled);
                }
                Some(rejected) => {
                    self.balancer.report(answered.ticket, &rejected);
                    return Ok(Synced::Stalled);
                }
            }
        }
        Ok(Synced::Caught)
    }

    /// `None` = its claim is on our best chain; else the first height to fetch
    fn first_needed(&self, claim: BlockRef, reach: Option<Height>) -> Option<Height> {
        let verified = self.verified.borrow();
        let Some(verified) = verified.as_ref() else { return Some(Height::GENESIS) };
        if verified.hash_at(claim.height) == Some(claim.hash) {
            return None;
        }
        let floor = above_final(verified.final_tip());
        Some(reach.map_or(floor, |reach| reach.next().max(floor)))
    }

    /// Stage A, then stage B on the blocking pool, the chain + `served` (the run's validator, top,
    /// poll) into the view, then finality; `Some` = the rule the run broke
    async fn insert(
        &mut self,
        headers: Vec<Header>,
        served: Served,
    ) -> Result<Option<Rejected>, HeaderStoreFailed> {
        let (run, cut) = stage_a(*self.chain().params(), headers).await;
        let refused = self.blocking(move |chain| stage_b(chain, run, unix_now())).await;
        self.publish(Some(served));
        self.finalize().await?;
        Ok(refused.or(cut))
    }

    /// Finalizes what the view's holders allow, then publishes the move
    async fn finalize(&mut self) -> Result<(), HeaderStoreFailed> {
        let view = self.core.current();
        self.finality_paused =
            self.blocking(move |chain| finalize_held(chain, &view.holders)).await?;
        self.publish(None);
        Ok(())
    }

    fn chain(&self) -> &HeaderChain {
        self.chain.as_ref().expect("header chain back from the blocking pool")
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

    /// `VerifiedChain` → watch when its best or final tip moved; → view on that, a `served` run
    /// or a finality edge (an unchanged publish would wake this task's own wait)
    fn publish(&mut self, served: Option<Served>) {
        let chain = self.chain();
        let report = (chain.best().map(|best| best.block), chain.final_tip(), self.finality_paused);
        let moved =
            |old: &VerifiedChain| (Some(old.best()), old.final_tip()) != (report.0, report.1);
        let verified = chain.verified().map(Arc::new);
        self.verified.send_if_modified(|published| {
            let changed = published.as_deref().is_none_or(moved) && verified.is_some();
            if changed {
                *published = verified;
            }
            changed
        });
        if self.reported != Some(report) || served.is_some() {
            self.reported = Some(report);
            let verified = self.verified.borrow().clone();
            let finality_paused = self.finality_paused;
            self.core.apply_headers(HeaderReport { verified, served, finality_paused });
        }
    }
}

/// First height above `final_tip` (genesis while nothing is final)
fn above_final(final_tip: Option<BlockRef>) -> Height {
    final_tip.map_or(Height::GENESIS, |tip| tip.height.next())
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

/// Finalizes while some trusted validator holds the boundary (H6); `true` = finality paused
///
/// - `holders` = the view's, under this chain's best (published before every call)
/// - work never gates it (peers alone never finalize: a trusted holder = required)
fn finalize_held(chain: &mut HeaderChain, holders: &Holders) -> Result<bool, HeaderStoreFailed> {
    while let Some(boundary) = chain.finalizable() {
        if holders.holders(boundary).is_empty() {
            return Ok(true);
        }
        chain.finalize(boundary)?;
        if u32::from(boundary.height) % 100_000 < HEADER_BATCH {
            info!(height = u32::from(boundary.height), "Headers verified and final");
        }
    }
    Ok(false)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64
}
