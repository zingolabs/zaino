//! One validator's poller: read, diff, report.
//!
//! One tick = one poll batch (tip + listing + what it holds of the verified chain, metadata every
//! `METADATA_REFRESH`), one bytes batch for what *this endpoint* added, one report into the fold.
//! The diff is against this poller's own previous listing, so it reports `O(change)` rather than
//! a whole mempool per endpoint per tick.
//!
//! Each poller owns its interval, backoff and failure count, so a slow or dead validator
//! degrades alone (`docs/design/chainview.md` §7).
//!
//! # A transaction leaves one endpoint by exactly one route
//!
//! Mined or evicted, the validator stops listing it, so it falls out of the diff as a removal.
//! There is no second removal path keyed on blocks — the poller never reads a block body — and a
//! new tip does *not* clear the listing: an unmined transaction survives the block that did not
//! contain it, and re-fetching its bytes would be work for nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
// tokio's clock (paused-runtime tests advance the cadences)
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use zaino_primitives::types::{BlockHash, BlockRef, Height, NodeRelease, PeerInfo, TransactionId};
use zaino_source::{
    GetBlockError, GetMempoolListingError, GetRawMempoolTransactionError, MempoolListed,
    MetadataReading, NonDomainError, PollReading, QueryError,
};

use crate::config::{
    CATCHING_UP_WARN_INTERVAL, INITIAL_BACKOFF, MAX_BACKOFF, MAX_CONSECUTIVE_FAILURES,
    METADATA_REFRESH, MIN_POLL_SPACING, POLL_INTERVAL, STREAMED_POLL_INTERVAL,
};
use crate::endpoints::EndpointIndex;
use crate::error::EndpointPollError;
use crate::fold::{ChainViewCore, EndpointReport, Listing, Reading, Sighted};
use zaino_source::ChainDataSource;

/// What one tick found
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Polled {
    Listed(usize),
    /// `network` = the validator's own estimate of the network tip
    CatchingUp {
        tip: BlockRef,
        network: Height,
    },
}

/// The poll loop for one configured endpoint.
///
/// Construction does no I/O — an empty mempool is a valid answer — so the fold's first published
/// snapshot is empty with no tip.
pub struct EndpointPoller<S: ChainDataSource> {
    index: EndpointIndex,
    address: String,
    source: Arc<S>,
    view: Arc<ChainViewCore>,
    /// What this endpoint listed last tick — the diff's left side.
    listed: std::sync::Mutex<BTreeSet<TransactionId>>,
    metadata_read_at: std::sync::Mutex<Option<Instant>>,
    waker: PollWaker,
}

/// Wakes one poller early: a push stream's events, or anything else that knows a change happened
///
/// - `wake` coalesces: one pending wake at most, `MIN_POLL_SPACING` between polls
/// - streaming: reconcile every `STREAMED_POLL_INTERVAL` instead of `POLL_INTERVAL`; either edge
///   polls at once (events may have fallen in the gap)
#[derive(Debug, Clone, Default)]
pub struct PollWaker(Arc<Wake>);

#[derive(Debug, Default)]
struct Wake {
    notify: Notify,
    streaming: AtomicBool,
}

impl PollWaker {
    pub fn wake(&self) {
        self.0.notify.notify_one();
    }

    pub fn streaming(&self, up: bool) {
        self.0.streaming.store(up, Ordering::Relaxed);
        self.0.notify.notify_one();
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.0.streaming.load(Ordering::Relaxed)
    }

    /// Until the next poll: the floor, then the interval or a wake (`Break` = cancelled)
    async fn wait(&self, cancel: &CancellationToken) -> ControlFlow<()> {
        sleep_or_cancel(MIN_POLL_SPACING, cancel).await?;
        let interval = match self.is_streaming() {
            true => STREAMED_POLL_INTERVAL,
            false => POLL_INTERVAL,
        };
        let woken = async {
            tokio::select! {
                () = tokio::time::sleep(interval.saturating_sub(MIN_POLL_SPACING)) => {}
                () = self.0.notify.notified() => {}
            }
        };
        match cancel.run_until_cancelled(woken).await {
            Some(()) => ControlFlow::Continue(()),
            None => ControlFlow::Break(()),
        }
    }
}

impl<S: ChainDataSource> std::fmt::Debug for EndpointPoller<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointPoller")
            .field("index", &self.index.get())
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl<S: ChainDataSource> EndpointPoller<S> {
    pub(crate) fn new(
        index: EndpointIndex,
        address: String,
        source: Arc<S>,
        view: Arc<ChainViewCore>,
    ) -> Self {
        Self {
            index,
            address,
            source,
            view,
            listed: std::sync::Mutex::new(BTreeSet::new()),
            metadata_read_at: std::sync::Mutex::new(None),
            waker: PollWaker::default(),
        }
    }

    /// This poller's wake handle (a push stream's subscriber holds one)
    pub fn waker(&self) -> PollWaker {
        self.waker.clone()
    }

    /// One poll
    #[instrument(name = "EndpointPoller::tick", skip_all, fields(endpoint = %self.address))]
    pub(crate) async fn tick(&self) -> Result<Polled, EndpointPollError> {
        let started = Instant::now();
        let due = self
            .metadata_read_at
            .lock()
            .expect("metadata mutex poisoned")
            .is_none_or(|at| at.elapsed() >= METADATA_REFRESH);
        let asked = self.view.current().holders.asked();
        let PollReading { info, listing, held, metadata } =
            self.source.get_poll_reading(due, &asked).await.map_err(EndpointPollError::Source)?;
        let latency = started.elapsed();
        let (peers, release) = self.metadata(metadata);

        let tip = BlockRef { hash: info.best_block_hash, height: info.blocks };
        let network = info.estimated_height;
        let held = self.answered(&asked, held);
        let streaming = self.waker.is_streaming();
        let reading = Reading { held, info, latency, peers, release, streaming };

        let listing: BTreeMap<TransactionId, MempoolListed> = match listing {
            Ok(entries) => entries.into_iter().map(|entry| (entry.txid, entry)).collect(),
            Err(GetMempoolListingError::Unavailable) => return Err(EndpointPollError::Unavailable),
            Err(GetMempoolListingError::Inactive) => {
                self.view.apply(self.index, EndpointReport::CatchingUp(reading));
                self.listed.lock().expect("endpoint listing mutex poisoned").clear();
                return Ok(Polled::CatchingUp { tip, network });
            }
        };

        let previous = self.listed.lock().expect("endpoint listing mutex poisoned").clone();
        let removed: Vec<TransactionId> =
            previous.iter().filter(|txid| !listing.contains_key(txid)).copied().collect();

        let mut added: Vec<Sighted> = Vec::new();
        let mut admitted: BTreeSet<TransactionId> =
            listing.keys().filter(|txid| previous.contains(txid)).copied().collect();
        let mut unheld: Vec<MempoolListed> = Vec::new();
        for entry in listing.values().filter(|entry| !previous.contains(&entry.txid)) {
            // Fetch-once: bytes paid only by the first endpoint to report (§5)
            if self.view.holds(&entry.txid) {
                added.push(Sighted { txid: entry.txid, raw: None, fee: entry.fee });
                admitted.insert(entry.txid);
            } else {
                unheld.push(*entry);
            }
        }
        let fetched = if unheld.is_empty() {
            Vec::new()
        } else {
            let fetch = self.source.get_raw_mempool_transactions(&unheld);
            fetch.await.map_err(EndpointPollError::Source)?
        };
        for (entry, raw) in unheld.iter().zip(fetched) {
            match raw {
                Ok(raw) => {
                    let raw = Some(Bytes::from(raw));
                    added.push(Sighted { txid: entry.txid, raw, fee: entry.fee });
                    admitted.insert(entry.txid);
                }
                // listed then mined/evicted before the fetch (the race the port documents)
                Err(GetRawMempoolTransactionError::NotFound(txid)) => {
                    debug!(%txid, "Mempool transaction gone before fetch")
                }
            }
        }

        let unadmitted = self
            .view
            .apply(self.index, EndpointReport::Observed(reading, Listing { added, removed }));
        for txid in unadmitted {
            admitted.remove(&txid);
        }

        let size = admitted.len();
        *self.listed.lock().expect("endpoint listing mutex poisoned") = admitted;
        Ok(Polled::Listed(size))
    }

    /// Its best-chain blocks at the `asked` heights (above its tip or an item failed = no fact)
    fn answered(
        &self,
        asked: &[Height],
        held: Vec<Result<BlockHash, QueryError<GetBlockError>>>,
    ) -> Vec<BlockRef> {
        let answers = asked.iter().zip(held).filter_map(|(height, answer)| match answer {
            Ok(hash) => Some(BlockRef { hash, height: *height }),
            Err(QueryError::Domain(GetBlockError::HeightNotFound(_))) => None,
            Err(QueryError::NonDomain(cause)) => {
                debug!(endpoint = %self.address, ?height, %cause, "getblockhash unanswered");
                None
            }
        });
        answers.collect()
    }

    /// Telemetry halves of a metadata tick: a failed half keeps the last answer (`None`), warned
    ///
    /// - read time recorded on any answered metadata tick (a failed half waits a full refresh)
    fn metadata(
        &self,
        metadata: Option<MetadataReading>,
    ) -> (Option<Vec<PeerInfo>>, Option<NodeRelease>) {
        let Some(MetadataReading { peers, release }) = metadata else { return (None, None) };
        *self.metadata_read_at.lock().expect("metadata mutex poisoned") = Some(Instant::now());
        (kept(&self.address, "peer list", peers), kept(&self.address, "release", release))
    }

    /// A failed tick reported: `Failed` (degraded, holds no tip), or past the ceiling / with no
    /// mempool `Down` (sightings retracted too); `true` = down
    pub(crate) fn failed(&self, error: &EndpointPollError, consecutive: u32) -> bool {
        let ejected = matches!(error, EndpointPollError::Unavailable)
            || consecutive >= MAX_CONSECUTIVE_FAILURES;
        if ejected {
            warn!(
                endpoint = %self.address, %error, attempts = consecutive, retry = ?MAX_BACKOFF,
                "Validator down, holds no tip",
            );
            self.view.apply(self.index, EndpointReport::Down);
            // the next answer must re-report everything listed
            self.listed.lock().expect("endpoint listing mutex poisoned").clear();
        } else {
            warn!(endpoint = %self.address, %error, attempts = consecutive, "Validator poll failed");
            self.view.apply(self.index, EndpointReport::Failed { consecutive });
        }
        ejected
    }

    /// Poll until `cancel`; a validator failing never ends it (serving stops only when no one holds
    /// the verified tip)
    ///
    /// - mempool inactive → chain still read, warned once per `CATCHING_UP_WARN_INTERVAL`
    /// - transport failure → backoff + retry, last observation kept (`EndpointReport::Failed`)
    /// - failure ceiling / no mempool → `Down` (sightings + chain retracted: never held stale),
    ///   still retried at `MAX_BACKOFF`; the next answer brings it back
    pub async fn run(self, cancel: CancellationToken) {
        let mut backoff = INITIAL_BACKOFF;
        let mut consecutive_failures = 0u32;
        let mut announced_ready = false;
        let mut down = false;
        let mut catching_up_warned: Option<Instant> = None;

        loop {
            let Some(outcome) = cancel.run_until_cancelled(self.tick()).await else {
                return;
            };

            match outcome {
                Ok(polled) => {
                    consecutive_failures = 0;
                    backoff = INITIAL_BACKOFF;
                    if std::mem::take(&mut down) {
                        info!(endpoint = %self.address, "Validator back");
                    }
                    match polled {
                        Polled::Listed(size) => {
                            if catching_up_warned.take().is_some() {
                                info!(endpoint = %self.address, mempool = size, "Validator caught up");
                            } else if !announced_ready {
                                info!(endpoint = %self.address, mempool = size, "Polling validator");
                            }
                            announced_ready = true;
                        }
                        Polled::CatchingUp { tip, network } => {
                            if catching_up_warned
                                .is_none_or(|at| at.elapsed() >= CATCHING_UP_WARN_INTERVAL)
                            {
                                warn!(
                                    endpoint = %self.address,
                                    height = u32::from(tip.height),
                                    behind = u32::from(network).saturating_sub(tip.height.into()),
                                    hash = %tip.hash,
                                    "Validator catching up",
                                );
                                catching_up_warned = Some(Instant::now());
                            }
                        }
                    }
                    if self.waker.wait(&cancel).await.is_break() {
                        return;
                    }
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if !down {
                        down = self.failed(&error, consecutive_failures);
                    }
                    let delay = if down { MAX_BACKOFF } else { backoff };
                    if sleep_or_cancel(delay, &cancel).await.is_break() {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

/// A telemetry read's answer, or `None` (last kept) with a warning
fn kept<T>(endpoint: &str, what: &str, read: Result<T, NonDomainError>) -> Option<T> {
    read.inspect_err(|cause| warn!(endpoint, %cause, "Validator {what} read failed, last kept"))
        .ok()
}

async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) -> ControlFlow<()> {
    match cancel.run_until_cancelled(tokio::time::sleep(delay)).await {
        Some(()) => ControlFlow::Continue(()),
        None => ControlFlow::Break(()),
    }
}
