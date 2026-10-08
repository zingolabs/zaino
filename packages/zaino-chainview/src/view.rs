//! The aggregate: the write handle that submits transactions, and the read handles.

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, warn};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::{ReorgDepth, TransactionId};
use zaino_source::{
    prepare_transaction, ChainDataSource, FailureMode, NonDomainError, Prepared, QueryError,
    SendRawTransactionError,
};
use zaino_traffic::{MemberId, TrafficBalancer, ValidatorId};

use crate::endpoints::ValidatorMetadata;
use crate::error::{ConfigError, SubmitError};
use crate::fold::ChainViewCore;
use crate::headers::HeaderSync;
use crate::observe::ObservationFold;
use crate::peers::PeerWatch;
use crate::ports::ValidatorP2pSource;
use crate::snapshot::ChainViewSnapshot;
use crate::submit::{Ended, Entry, Job, Pushed, Seen, Step, SubmitPolicy};
use crate::telemetry;

/// One view over N validators.
///
/// The *write* side: it owns submission, the only operation that mutates the chain. Everything
/// else holds a [`ChainViewSubscriber`].
pub struct ChainView<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    addresses: Vec<String>,
    policy: SubmitPolicy,
    peers: Option<Arc<dyn ValidatorP2pSource>>,
}

impl<S> std::fmt::Debug for ChainView<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainView").field("core", &self.core).finish_non_exhaustive()
    }
}

impl<S: ChainDataSource> ChainView<S> {
    /// The view over `balancer`'s trusted members (`addresses[i]` = `ValidatorId(i)`'s, for logs
    /// and status); its tip comes from [`header_sync`](Self::header_sync), its polls from
    /// [`observation_fold`](Self::observation_fold)
    ///
    /// - `depth` = the header chain's (sync's `finalised_depth`): the finality stall alarm's
    /// - each poll asks `getblockhash` at this view's best ([`TrafficBalancer::poll_best`])
    pub fn new(
        addresses: Vec<String>,
        balancer: TrafficBalancer<S>,
        depth: ReorgDepth,
    ) -> Result<Self, ConfigError> {
        if addresses.is_empty() {
            return Err(ConfigError::NoEndpoints);
        }
        if addresses.len() > ValidatorId::MAX {
            return Err(ConfigError::TooManyEndpoints { count: addresses.len() });
        }
        let members = balancer.members().borrow().clone();
        let trusted = members.rows.iter().filter(|row| matches!(row.id, MemberId::Trusted(_)));
        assert_eq!(trusted.count(), addresses.len(), "an address per trusted member");
        let endpoints = addresses.iter().map(|address| ValidatorMetadata::new(address.clone()));
        let core = Arc::new(ChainViewCore::new(endpoints.collect(), depth));
        let asked = Arc::clone(&core);
        balancer.poll_best(move || asked.current().best().map(|best| best.height));
        Ok(Self { core, balancer, addresses, policy: SubmitPolicy::default(), peers: None })
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

    /// Stands in for header sync in tests: `verified` as its word
    #[cfg(any(test, feature = "testing"))]
    pub fn set_verified(&self, verified: Option<zaino_header_chain::VerifiedChain>) {
        self.core.apply_headers(verified.map(Arc::new));
    }

    /// The runnable that feeds `chain` from the validators and publishes its best tip into this
    /// view
    pub fn header_sync(&self, chain: HeaderChain) -> HeaderSync<S> {
        HeaderSync::new(Arc::clone(&self.core), self.balancer.clone(), chain)
    }

    /// The runnable folding each trusted member's polls into this view
    pub fn observation_fold(&self) -> ObservationFold<S> {
        let (core, balancer) = (Arc::clone(&self.core), self.balancer.clone());
        ObservationFold { core, balancer, addresses: self.addresses.clone() }
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
            balancer: self.balancer.clone(),
            peers: self.peers.clone(),
            raw: Bytes::from(raw),
            txid: prepared.txid,
        };
        tokio::spawn(job.run(self.policy, answer));
        answered.await.unwrap_or_else(|_| {
            let gone = NonDomainError::new(FailureMode::Connection, "submission ended");
            Err(SubmitError::Unreachable { attempted: 0, cause: gone })
        })
    }
}

/// Refusals the bytes and the tip decide alone (no push: a peer would drop it silently)
///
/// - expired: ZIP-203, invalid in any block above its expiry height (next block = tip + 1)
/// - wrong branch: v5+ embeds the branch it was signed for; the next block's must match
///
/// - code `-25` = zebrad's own for both (consensus-invalid)
fn precheck(view: &ChainViewSnapshot, prepared: &Prepared) -> Result<(), SendRawTransactionError> {
    let invalid = |message: String| SendRawTransactionError::Rejected { code: -25, message };
    let Some(info) = view.validator_info() else { return Ok(()) };
    let next = info.blocks.next();
    if let Some(expiry) = prepared.expiry_height.filter(|&expiry| next > expiry) {
        return Err(invalid(format!(
            "tx-expiring-soon: expiry height {} is below the next block {}",
            u32::from(expiry),
            u32::from(next)
        )));
    }
    if let Some(branch) = prepared.branch.filter(|&branch| branch != info.consensus.next_block) {
        return Err(invalid(format!(
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
    balancer: TrafficBalancer<S>,
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
        let trusted = self.balancer.entries();
        let mut job = Job::new(entries, trusted, policy, fastrand::Rng::new());
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
                    NonDomainError::new(FailureMode::Connection, "no validator to push to")
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
            (Entry::Trusted(member), _) => {
                match self.balancer.submit(member, self.raw.to_vec()).await {
                    Ok(_) => Pushed::Accepted,
                    Err(unanswered) => match unanswered.last {
                        Some(QueryError::Domain(rejected)) => Pushed::Rejected(rejected),
                        Some(QueryError::NonDomain(cause)) => Pushed::Unreachable(cause),
                        None => {
                            let out = NonDomainError::new(
                                FailureMode::Connection,
                                unanswered.to_string(),
                            );
                            Pushed::Unreachable(out)
                        }
                    },
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

    /// Every publish (one per fold): [`current`](Self::current) has moved
    pub fn subscribe_published(&self) -> tokio::sync::watch::Receiver<()> {
        self.core.subscribe_published()
    }
}
