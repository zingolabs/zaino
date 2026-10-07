//! The aggregate: the write handle that submits transactions, and the read handles.

use std::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, warn};
use zaino_primitives::types::{ReorgDepth, TransactionId};
use zaino_source::{
    prepare_transaction, ChainDataSource, NonDomainError, Prepared, QueryError,
    SendRawTransactionError,
};

use zaino_header_chain::HeaderChain;

use crate::endpoint::EndpointPoller;
use crate::endpoints::{EndpointIndex, EndpointState, ValidatorMetadata};
use crate::error::{ConfigError, SubmitError};
use crate::feed::MempoolTail;
use crate::fold::ChainViewCore;
use crate::headers::HeaderSync;
use crate::peers::PeerWatch;
use crate::ports::ValidatorP2pSource;
use crate::snapshot::ChainViewSnapshot;
use crate::submit::{Ended, Entry, Job, Pushed, Seen, Step, SubmitPolicy};
use crate::telemetry;
use crate::tip::{ChainTip, Unserved};

/// One operator-configured trusted validator (trust is configured, never discovered: §1)
pub struct Endpoint<S: ChainDataSource> {
    /// For logs only
    pub address: String,
    pub source: Arc<S>,
}

/// One view over N validators.
///
/// The *write* side: it owns submission, the only operation that mutates the chain. Everything
/// else holds a [`ChainViewSubscriber`].
pub struct ChainView<S: ChainDataSource> {
    core: Arc<ChainViewCore>,
    sources: Arc<Vec<Arc<S>>>,
    policy: SubmitPolicy,
    peers: Option<Arc<dyn ValidatorP2pSource>>,
}

impl<S: ChainDataSource> std::fmt::Debug for ChainView<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainView").field("core", &self.core).finish_non_exhaustive()
    }
}

impl<S: ChainDataSource> ChainView<S> {
    /// The view + one runnable poller per endpoint (index = position in `endpoints`); its tip
    /// comes from [`header_sync`](Self::header_sync)
    ///
    /// - `depth` = the header chain's (sync's `finalised_depth`): each poll asks who holds the
    ///   final boundary, `depth` below the best
    pub fn new(
        endpoints: Vec<Endpoint<S>>,
        depth: ReorgDepth,
    ) -> Result<(Self, Vec<EndpointPoller<S>>), ConfigError> {
        let configured = NonZeroUsize::new(endpoints.len()).ok_or(ConfigError::NoEndpoints)?;
        if configured.get() > crate::EndpointSet::MAX {
            return Err(ConfigError::TooManyEndpoints { count: configured.get() });
        }

        let core = Arc::new(ChainViewCore::new(
            endpoints
                .iter()
                .map(|endpoint| ValidatorMetadata::new(endpoint.address.clone()))
                .collect(),
            depth,
        ));
        let mut sources = Vec::with_capacity(endpoints.len());
        let mut pollers = Vec::with_capacity(endpoints.len());
        for (position, endpoint) in endpoints.into_iter().enumerate() {
            let index = EndpointIndex::new(position).expect("position below EndpointSet::MAX");
            sources.push(Arc::clone(&endpoint.source));
            pollers.push(EndpointPoller::new(
                index,
                endpoint.address,
                endpoint.source,
                Arc::clone(&core),
            ));
        }

        let sources = Arc::new(sources);
        Ok((Self { core, sources, policy: SubmitPolicy::default(), peers: None }, pollers))
    }

    /// How hard each submission pushes (default: 15 s threshold, 4 attempts)
    pub fn with_submit_policy(self, policy: SubmitPolicy) -> Self {
        Self { policy, ..self }
    }

    /// Peers as submission entries + announcers (§5, §6); spawn [`peer_watch`](Self::peer_watch)
    pub fn with_peers(self, peers: Arc<dyn ValidatorP2pSource>) -> Self {
        Self { peers: Some(peers), ..self }
    }

    /// The runnable folding peers' announcements into the view (`None` = no peers)
    pub fn peer_watch(&self) -> Option<PeerWatch> {
        let peers = Arc::clone(self.peers.as_ref()?);
        Some(PeerWatch::new(Arc::clone(&self.core), peers))
    }

    /// Stands in for header sync in tests: `verified` as its word, no run served, finality on
    #[cfg(any(test, feature = "testing"))]
    pub fn set_verified(&self, verified: Option<zaino_header_chain::VerifiedChain>) {
        let verified = verified.map(Arc::new);
        let report = crate::fold::HeaderReport { verified, served: None, finality_paused: false };
        self.core.apply_headers(report);
    }

    /// The runnable that feeds `chain` from the validators and publishes its best tip into this
    /// view; `sources` = the same validators, configured order (their bulk lane)
    pub fn header_sync(&self, chain: HeaderChain, sources: Vec<Arc<S>>) -> HeaderSync<S> {
        HeaderSync::new(Arc::clone(&self.core), sources, chain)
    }

    /// A reader onto the same published cell and action channel.
    pub fn subscriber(&self) -> ChainViewSubscriber {
        ChainViewSubscriber { core: Arc::clone(&self.core) }
    }

    /// Submit one transaction (§6): precheck, then one random entry per attempt, watched
    ///
    /// - answers at the first acceptance (the transaction is `ours` from then); a rejection or
    ///   a failure only once every attempt is spent
    /// - the job outlives the answer: it keeps resubmitting until the transaction spreads
    pub async fn submit(&self, raw: Vec<u8>) -> Result<TransactionId, SubmitError> {
        let prepared = prepare_transaction(&raw).map_err(|malformed| {
            SubmitError::Rejected(SendRawTransactionError::Malformed(malformed.to_string()))
        })?;
        precheck(&self.core.current(), &prepared).map_err(SubmitError::Rejected)?;

        let (answer, answered) = oneshot::channel();
        let job = Submission {
            core: Arc::clone(&self.core),
            sources: Arc::clone(&self.sources),
            peers: self.peers.clone(),
            raw: Bytes::from(raw),
            txid: prepared.txid,
        };
        tokio::spawn(job.run(self.policy, answer));
        answered.await.unwrap_or_else(|_| {
            let gone =
                NonDomainError::new(zaino_source::FailureMode::Connection, "submission ended");
            Err(SubmitError::Unreachable { attempted: 0, cause: gone })
        })
    }
}

/// Refusals the bytes and the tip decide alone (no push: a peer would drop it silently)
///
/// - expired: ZIP-203, invalid in any block above its expiry height (next block = tip + 1)
/// - wrong branch: v5+ embeds the branch it was signed for; the next block's must match
fn precheck(view: &ChainViewSnapshot, prepared: &Prepared) -> Result<(), SendRawTransactionError> {
    let Ok(info) = view.validator_info() else { return Ok(()) };
    let next = info.blocks.next();
    if let Some(expiry) = prepared.expiry_height.filter(|&expiry| next > expiry) {
        return Err(SendRawTransactionError::Rejected(format!(
            "tx-expiring-soon: expiry height {} is below the next block {}",
            u32::from(expiry),
            u32::from(next)
        )));
    }
    if let Some(branch) = prepared.branch.filter(|&branch| branch != info.consensus.next_block) {
        return Err(SendRawTransactionError::Rejected(format!(
            "built for consensus branch {branch}, the next block's is {}",
            info.consensus.next_block
        )));
    }
    Ok(())
}

/// What the wallet hears back from one submission
type SubmitAnswer = Result<TransactionId, SubmitError>;

/// One running submission (spawned: it outlives the wallet's answer)
struct Submission<S> {
    core: Arc<ChainViewCore>,
    sources: Arc<Vec<Arc<S>>>,
    peers: Option<Arc<dyn ValidatorP2pSource>>,
    raw: Bytes,
    txid: TransactionId,
}

impl<S: ChainDataSource> Submission<S> {
    async fn run(self, policy: SubmitPolicy, answer: oneshot::Sender<SubmitAnswer>) {
        let mut answer = Some(answer);
        let mut published = self.core.subscribe_published();
        let tip = self.core.current().best().map(|best| best.height);
        let entries = self.peers.as_ref().map(|peers| peers.entries(tip)).unwrap_or_default();
        let mut job = Job::new(entries, self.trusted(), policy, fastrand::Rng::new());
        loop {
            let step = job.step(Instant::now(), self.seen(job.tried()));
            // after `step`: one observation can both accept and end the job
            if job.accepted() {
                self.answer_accepted(&mut answer);
            }
            match step {
                Step::Push(entry) => {
                    let outcome = self.push(entry).await;
                    debug!(txid = %self.txid, ?entry, ?outcome, "Submission pushed");
                    job.pushed(outcome, Instant::now());
                }
                Step::Wait(until) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(until) => {}
                        _ = published.changed() => {}
                    }
                }
                Step::Done => break,
            }
        }

        let tried = job.tried().to_vec();
        let attempts = tried.len();
        let ended = job.ended(self.seen(&tried));
        telemetry::submitted(&ended, attempts);
        let verdict = match ended {
            Ended::Spread | Ended::Unconfirmed => return,
            Ended::Rejected(rejected) => SubmitError::Rejected(rejected),
            Ended::Unreachable(cause) => SubmitError::Unreachable {
                attempted: attempts,
                cause: cause.unwrap_or_else(|| {
                    NonDomainError::new(
                        zaino_source::FailureMode::Connection,
                        "no validator to push to",
                    )
                }),
            },
        };
        if let Some(answer) = answer.take() {
            warn!(txid = %self.txid, attempts, %verdict, "Transaction not accepted");
            let _ = answer.send(Err(verdict));
        }
    }

    /// First acceptance: `ours` (servable at once), then the wallet's `Ok`
    fn answer_accepted(&self, answer: &mut Option<oneshot::Sender<SubmitAnswer>>) {
        if let Some(answer) = answer.take() {
            self.core.mark_ours(self.txid, self.raw.clone());
            let _ = answer.send(Ok(self.txid));
        }
    }

    async fn push(&self, entry: Entry) -> Pushed {
        match (entry, &self.peers) {
            (Entry::Trusted(index), _) => {
                let source = &self.sources[index.get()];
                match source.send_raw_transaction(self.raw.to_vec()).await {
                    Ok(_) => Pushed::Accepted,
                    Err(QueryError::Domain(rejected)) => Pushed::Rejected(rejected),
                    Err(QueryError::NonDomain(cause)) => Pushed::Unreachable(cause),
                }
            }
            (Entry::Peer(addr), Some(peers)) => match peers.push(addr, self.raw.clone()).await {
                Ok(()) => Pushed::Delivered,
                Err(cause) => Pushed::Unreachable(cause),
            },
            (Entry::Peer(_), None) => {
                unreachable!("peer entries come only from a ValidatorP2pSource")
            }
        }
    }

    /// Every trusted validator not `Down` (all of them when every one is)
    fn trusted(&self) -> Vec<EndpointIndex> {
        let view = self.core.current();
        let all = (0..self.sources.len()).filter_map(EndpointIndex::new);
        let up: Vec<_> = all
            .clone()
            .filter(|i| view.endpoints()[i.get()].state != EndpointState::Down)
            .collect();
        if up.is_empty() {
            all.collect()
        } else {
            up
        }
    }

    /// The transaction as the view shows it, against the entries tried so far
    fn seen(&self, tried: &[Entry]) -> Seen {
        let view = self.core.current();
        let listing = view.sighting(&self.txid).map(|s| s.trusted()).unwrap_or_default();
        let announcers = view.announcers(&self.txid);
        Seen::of(listing, view.mempool_readers(), &announcers, &view.peers_live, tried)
    }
}

/// A cheap-to-clone reader for a running view.
///
/// Holds no way to drive polling or to relay a transaction — both stay on
/// [`ChainView`](crate::ChainView).
#[derive(Clone)]
pub struct ChainViewSubscriber {
    core: Arc<ChainViewCore>,
}

impl std::fmt::Debug for ChainViewSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainViewSubscriber").field("core", &self.core).finish_non_exhaustive()
    }
}

impl ChainViewSubscriber {
    /// The most recently published view. One atomic load — pin it once per request or stream
    /// and ask it everything, rather than calling this per answer.
    pub fn current(&self) -> Arc<ChainViewSnapshot> {
        self.core.current()
    }

    /// Latest tip, level-triggered (`None` = unserved): what block sync follows
    pub fn subscribe_tip(&self) -> tokio::sync::watch::Receiver<Option<ChainTip>> {
        self.core.subscribe_tip()
    }

    /// One `GetMempoolStream`: the servable mempool at the current tip block, then each
    /// arrival, until the block moves (no tip = the refusal)
    ///
    /// - wake subscribed **before** the epoch is read (a fold landing between = one spurious
    ///   wake, never a missed arrival)
    pub fn tail(&self) -> Result<MempoolTail, Unserved> {
        let wake = self.core.subscribe_tails();
        Ok(MempoolTail::new(self.core.epoch()?, wake))
    }
}
