//! One validator's poller: read, diff, report.
//!
//! One tick = readiness, tip + its ancestry (headers only, `O(new blocks)`), peers every
//! `PEER_REFRESH`, listing, bytes for what *this endpoint* added, one report into the fold. The
//! diff is against this poller's own previous listing, so it reports `O(change)` rather than a
//! whole mempool per endpoint per tick.
//!
//! Each poller owns its interval, backoff and failure count, so a slow or dead validator
//! degrades alone (`docs/design/chainview.md` §2).
//!
//! # A transaction leaves one endpoint by exactly one route
//!
//! Mined or evicted, the validator stops listing it, so it falls out of the diff as a removal.
//! There is no second removal path keyed on blocks — the poller never reads a block body — and a
//! new tip does *not* clear the listing: an unmined transaction survives the block that did not
//! contain it, and re-fetching its bytes would be work for nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use zaino_primitives::types::{BlockRef, Height, ReorgDepth, TransactionId, Zatoshis};
use zaino_source::{
    GetBlockLinkError, GetChainTipError, GetMempoolListingError, GetRawMempoolTransactionError,
    QueryError,
};

use crate::chain::{EndpointChain, Walk};
use crate::config::{
    CATCHING_UP_WARN_INTERVAL, INITIAL_BACKOFF, LINK_BATCH, LINK_FETCH_CONCURRENCY, MAX_BACKOFF,
    MAX_CONSECUTIVE_FAILURES, PEER_REFRESH, POLL_INTERVAL,
};
use crate::endpoints::EndpointIndex;
use crate::error::EndpointPollError;
use crate::fold::{ChainViewCore, EndpointReport, Listing, Reading, Sighted};
use crate::ports::EndpointSource;

/// What one tick found
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Polled {
    Listed(usize),
    /// `network` = the validator's own estimate of the network tip
    CatchingUp {
        tip: BlockRef,
        network: Height,
    },
    Syncing,
}

/// The poll loop for one configured endpoint.
///
/// Construction does no I/O — an empty mempool is a valid answer — so the fold's first published
/// snapshot is empty with no tip.
pub struct EndpointPoller<S: EndpointSource> {
    index: EndpointIndex,
    address: String,
    source: Arc<S>,
    view: Arc<ChainViewCore>,
    depth: ReorgDepth,
    /// What this endpoint listed last tick — the diff's left side.
    listed: std::sync::Mutex<BTreeSet<TransactionId>>,
    /// Its chain as of the last completed walk — the next walk's join point
    chain: std::sync::Mutex<Option<EndpointChain>>,
    peers_read_at: std::sync::Mutex<Option<Instant>>,
}

impl<S: EndpointSource> std::fmt::Debug for EndpointPoller<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointPoller")
            .field("index", &self.index.get())
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl<S: EndpointSource> EndpointPoller<S> {
    pub(crate) fn new(
        index: EndpointIndex,
        address: String,
        source: Arc<S>,
        view: Arc<ChainViewCore>,
        depth: ReorgDepth,
    ) -> Self {
        Self {
            index,
            address,
            source,
            view,
            depth,
            listed: std::sync::Mutex::new(BTreeSet::new()),
            chain: std::sync::Mutex::new(None),
            peers_read_at: std::sync::Mutex::new(None),
        }
    }

    /// One poll
    #[instrument(name = "EndpointPoller::tick", skip_all, fields(endpoint = %self.address))]
    pub(crate) async fn tick(&self) -> Result<Polled, EndpointPollError> {
        let started = Instant::now();
        // Readiness only (vote = the tip coherent with the listing, so it comes from
        // get_mempool_source_tip; that port is Infallible, so it cannot report NotReady)
        match self.source.get_chain_tip().await {
            Ok(_) => {}
            Err(QueryError::Domain(GetChainTipError::NotReady)) => {
                debug!("Validator not ready, vote withheld");
                self.view.apply(self.index, EndpointReport::Syncing);
                self.listed.lock().expect("endpoint listing mutex poisoned").clear();
                return Ok(Polled::Syncing);
            }
            Err(QueryError::NonDomain(cause)) => return Err(EndpointPollError::Source(cause)),
        }

        let source = match self.source.get_mempool_source_tip().await {
            Ok(source) => source,
            // `GetMempoolSourceTip` is typed `Infallible`: no domain rejection exists.
            Err(QueryError::Domain(never)) => match never {},
            Err(QueryError::NonDomain(cause)) => return Err(EndpointPollError::Source(cause)),
        };
        let tip = BlockRef { hash: source.hash, height: source.height };
        let chain = self.follow(tip).await?;
        let reading =
            |started: Instant, peers| Reading { chain, latency: started.elapsed(), peers };

        let listing: BTreeMap<TransactionId, Zatoshis> =
            match self.source.get_mempool_listing().await {
                Ok(entries) => entries.into_iter().map(|entry| (entry.txid, entry.fee)).collect(),
                Err(QueryError::Domain(GetMempoolListingError::Unavailable)) => {
                    return Err(EndpointPollError::Unavailable)
                }
                Err(QueryError::Domain(GetMempoolListingError::Inactive)) => {
                    self.view.apply(self.index, EndpointReport::CatchingUp(reading(started, None)));
                    self.listed.lock().expect("endpoint listing mutex poisoned").clear();
                    return Ok(Polled::CatchingUp { tip, network: source.estimated_height });
                }
                Err(QueryError::NonDomain(cause)) => return Err(EndpointPollError::Source(cause)),
            };

        let previous = self.listed.lock().expect("endpoint listing mutex poisoned").clone();
        let removed: Vec<TransactionId> =
            previous.iter().filter(|txid| !listing.contains_key(txid)).copied().collect();

        let mut added: Vec<Sighted> = Vec::new();
        let mut admitted: BTreeSet<TransactionId> =
            listing.keys().filter(|txid| previous.contains(txid)).copied().collect();
        for (txid, fee) in listing.iter().filter(|(txid, _)| !previous.contains(txid)) {
            // Fetch-once: bytes = a round trip, paid only by the first endpoint to report (§3)
            if self.view.holds(txid) {
                added.push(Sighted { txid: *txid, raw: None, fee: *fee });
                admitted.insert(*txid);
                continue;
            }
            match self.source.get_raw_mempool_transaction(*txid).await {
                Ok(raw) => {
                    added.push(Sighted { txid: *txid, raw: Some(Bytes::from(raw)), fee: *fee });
                    admitted.insert(*txid);
                }
                // Listed then mined/evicted before the fetch — the race the port documents.
                Err(QueryError::Domain(GetRawMempoolTransactionError::NotFound(_))) => {
                    debug!(%txid, "Mempool transaction gone before fetch")
                }
                Err(QueryError::NonDomain(cause)) => return Err(EndpointPollError::Source(cause)),
            }
        }

        let peers = self.read_peers().await?;
        let unadmitted = self.view.apply(
            self.index,
            EndpointReport::Observed(reading(started, peers), Listing { added, removed }),
        );
        for txid in unadmitted {
            admitted.remove(&txid);
        }

        let size = admitted.len();
        *self.listed.lock().expect("endpoint listing mutex poisoned") = admitted;
        Ok(Polled::Listed(size))
    }

    /// The endpoint's chain down from `tip`, walked onto the one held last tick
    ///
    /// - `getblockheader` by height, [`LINK_FETCH_CONCURRENCY`] in flight, each checked against
    ///   its child's `prev_hash`
    /// - endpoint reorged mid-walk → last tick's chain kept (a race, not a failure)
    async fn follow(&self, tip: BlockRef) -> Result<Option<EndpointChain>, EndpointPollError> {
        let held = self.chain.lock().expect("endpoint chain mutex poisoned").clone();
        let mut walk = Walk::new(tip, self.depth);
        loop {
            if let Some(chain) = walk.finish(held.as_ref()) {
                *self.chain.lock().expect("endpoint chain mutex poisoned") = Some(chain.clone());
                return Ok(Some(chain));
            }
            let lowest = match held.as_ref().map(|held| held.tip().height.next()) {
                // above the held tip: every height new
                Some(above) if above <= walk.next() => above,
                // under the held tip (a reorg): a batch at a time until it joins
                Some(_) => walk.next().saturating_sub(LINK_BATCH - 1),
                None => walk.lowest(),
            }
            .max(walk.lowest());
            let heights: Vec<Height> = lowest.up_to(walk.next()).collect();
            let mut links = futures::stream::iter(heights.into_iter().rev())
                .map(|height| self.source.get_block_link(height))
                .buffered(LINK_FETCH_CONCURRENCY);
            while let Some(link) = links.next().await {
                let link = match link {
                    Ok(link) => link,
                    Err(QueryError::Domain(GetBlockLinkError::HeightNotFound(height))) => {
                        debug!(?height, "Validator's tip retreated mid-walk, chain kept");
                        return Ok(held);
                    }
                    Err(QueryError::NonDomain(cause)) => {
                        return Err(EndpointPollError::Source(cause))
                    }
                };
                if walk.descend(link).is_err() {
                    debug!(?tip, "Validator reorged mid-walk, chain kept");
                    return Ok(held);
                }
                if walk.finish(held.as_ref()).is_some() {
                    break;
                }
            }
        }
    }

    /// `getpeerinfo` at its own slower cadence (a node refusing its peer list costs the view
    /// nothing)
    async fn read_peers(&self) -> Result<Option<Vec<String>>, EndpointPollError> {
        {
            let read_at = self.peers_read_at.lock().expect("peer refresh mutex poisoned");
            if read_at.is_some_and(|at| at.elapsed() < PEER_REFRESH) {
                return Ok(None);
            }
        }

        let peers = match self.source.get_peer_info().await {
            Ok(peers) => peers.into_iter().map(|peer| peer.addr).collect(),
            Err(QueryError::Domain(_)) => Vec::new(),
            Err(QueryError::NonDomain(cause)) => return Err(EndpointPollError::Source(cause)),
        };
        *self.peers_read_at.lock().expect("peer refresh mutex poisoned") = Some(Instant::now());
        Ok(Some(peers))
    }

    /// Poll until `cancel`
    ///
    /// - mempool inactive → tip still voted, warned once per `CATCHING_UP_WARN_INTERVAL`
    /// - transport failure → backoff + retry, last observation kept (`EndpointReport::Failed`)
    /// - failure ceiling / [`EndpointPollError::Unavailable`] → ejected (sightings + vote
    ///   retracted: fail closed, never vote stale) → `Err`
    pub async fn run(self, cancel: CancellationToken) -> Result<(), EndpointPollError> {
        let mut backoff = INITIAL_BACKOFF;
        let mut consecutive_failures = 0u32;
        let mut announced_ready = false;
        let mut catching_up_warned: Option<Instant> = None;

        loop {
            let Some(outcome) = cancel.run_until_cancelled(self.tick()).await else {
                return Ok(());
            };

            match outcome {
                Ok(polled) => {
                    consecutive_failures = 0;
                    backoff = INITIAL_BACKOFF;
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
                        Polled::Syncing => {}
                    }
                    if sleep_or_cancel(POLL_INTERVAL, &cancel).await.is_break() {
                        return Ok(());
                    }
                }
                Err(EndpointPollError::Unavailable) => {
                    warn!(endpoint = %self.address, "Validator has no mempool, ejected");
                    self.view.apply(self.index, EndpointReport::Down);
                    return Err(EndpointPollError::Unavailable);
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        warn!(
                            endpoint = %self.address,
                            %error,
                            attempts = consecutive_failures,
                            "Validator ejected after repeated failures",
                        );
                        self.view.apply(self.index, EndpointReport::Down);
                        return Err(error);
                    }
                    warn!(endpoint = %self.address, %error, attempts = consecutive_failures, "Validator poll failed");
                    self.view.apply(
                        self.index,
                        EndpointReport::Failed { consecutive: consecutive_failures },
                    );
                    if sleep_or_cancel(backoff, &cancel).await.is_break() {
                        return Ok(());
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) -> ControlFlow<()> {
    match cancel.run_until_cancelled(tokio::time::sleep(delay)).await {
        Some(()) => ControlFlow::Continue(()),
        None => ControlFlow::Break(()),
    }
}
