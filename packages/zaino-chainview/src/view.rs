//! Aggregate: submitting handle + read handles

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::FinalityDepth;
use zaino_source::{
    prepare_transaction, ChainDataSource, CheckAdmissionError, FailureMode, NonDomainError,
    Prepared, QueryError, SendRawTransactionError,
};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::TxId;

use crate::admission::{valid_in, NextBlock};
use crate::chain_view_snapshot::ChainViewSnapshot;
use crate::error::SubmitError;
use crate::headers::HeaderSync;
use crate::merge::ChainViewCore;
use crate::observe::ObservationMerge;
use crate::peer_mempool::PeerMempool;
use crate::peers::PeerWatch;
use crate::ports::{PeerSource, UnspentOutputs};
use crate::submit::{Ended, Entry, Job, Pushed, Seen, Step, SubmitPolicy};
use crate::telemetry;
use crate::trusted_validators::TrustedValidatorMeta;

/// One view over N validators: owns submission and the runnables feeding it
///
/// - readers hold a [`ChainViewSubscriber`]
pub struct ChainView<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    schedule: LocalNetwork,
    policy: SubmitPolicy,
    peers: Option<Arc<dyn PeerSource>>,
    shutdown: CancellationToken,
}

impl<S> std::fmt::Debug for ChainView<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainView").field("core", &self.core).finish_non_exhaustive()
    }
}

impl<S: ChainDataSource> ChainView<S> {
    /// View over `balancer`'s trusted members (named by [`TrafficBalancer::addresses`]): tip from
    /// [`header_sync`](Self::header_sync), polls from [`observation_merge`](Self::observation_merge)
    ///
    /// - `depth` = the header chain's (sync's `finalised_depth`): the finality stall alarm's
    /// - `schedule` = the network's: the branch a transaction must be signed for at the next block
    /// - `shutdown` cancels in-flight submissions (their callers read "submission ended")
    /// - each poll asks `getblockhash` at this view's best ([`TrafficBalancer::poll_best`])
    pub fn new(
        balancer: TrafficBalancer<S>,
        depth: FinalityDepth,
        schedule: LocalNetwork,
        shutdown: CancellationToken,
    ) -> Self {
        let trusted_validators =
            balancer.addresses().iter().map(|address| TrustedValidatorMeta::new(address.clone()));
        let core = Arc::new(ChainViewCore::new(trusted_validators.collect(), depth));
        let asked = Arc::clone(&core);
        balancer.poll_best(move || asked.current().best().map(|best| best.height));
        let policy = SubmitPolicy::default();
        Self { core, balancer, schedule, policy, peers: None, shutdown }
    }

    /// How hard each submission pushes (default: 15 s threshold, 4 attempts)
    pub fn with_submit_policy(self, policy: SubmitPolicy) -> Self {
        Self { policy, ..self }
    }

    /// Peers as submission entries, mempool listers and chain carriers
    ///
    /// - spawn [`peer_watch`](Self::peer_watch) and [`peer_mempool`](Self::peer_mempool)
    pub fn with_peers(self, peers: Arc<dyn PeerSource>) -> Self {
        Self { peers: Some(peers), ..self }
    }

    /// The runnable merging peers' block announcements into the view (`None` = no peers)
    pub fn peer_watch(&self) -> Option<PeerWatch> {
        let peers = Arc::clone(self.peers.as_ref()?);
        Some(PeerWatch::new(Arc::clone(&self.core), peers))
    }

    /// The runnable polling the peers' mempools into the view, admitting what a quorum holds
    /// and `utxos` funds (`None` = no peers)
    pub fn peer_mempool(&self, utxos: Arc<dyn UnspentOutputs>) -> Option<PeerMempool> {
        let peers = Arc::clone(self.peers.as_ref()?);
        Some(PeerMempool::new(Arc::clone(&self.core), peers, utxos, self.schedule))
    }

    /// Stands in for header sync in tests: `chain` as its word
    #[cfg(any(test, feature = "testing"))]
    pub fn set_chain(&self, chain: Option<zaino_header_chain::HeaderChainSnapshot>) {
        self.core.apply_headers(chain.map(Arc::new));
    }

    /// The runnable that feeds `chain` from the validators and publishes its best tip into this
    /// view
    pub fn header_sync(&self, chain: HeaderChain) -> HeaderSync<S> {
        HeaderSync::new(Arc::clone(&self.core), self.balancer.clone(), chain)
    }

    /// The runnable merging each trusted member's polls into this view
    pub fn observation_merge(&self) -> ObservationMerge<S> {
        let (core, balancer) = (Arc::clone(&self.core), self.balancer.clone());
        ObservationMerge { core, balancer }
    }

    /// Reader onto the same published cell
    pub fn subscriber(&self) -> ChainViewSubscriber {
        ChainViewSubscriber { core: Arc::clone(&self.core) }
    }

    /// Submit one transaction (relay job outlives the answer)
    /// - protocol: `docs/architecture.md` §6, API: `usage.md` "Submission"
    pub async fn submit(&self, raw: Vec<u8>) -> Result<TxId, SubmitError> {
        let prepared = prepare_transaction(&raw).map_err(|malformed| {
            SubmitError::Rejected(SendRawTransactionError::Malformed(malformed.to_string()))
        })?;
        precheck(&self.core.current(), &self.schedule, &prepared).map_err(SubmitError::Rejected)?;
        let checked = self.check(&raw).await.map_err(|refused| {
            telemetry::submitted(&Ended::Rejected(refused.clone()), 0);
            SubmitError::Rejected(refused)
        })?;

        let (answer, answered) = oneshot::channel();
        let job = Submission {
            core: Arc::clone(&self.core),
            balancer: self.balancer.clone(),
            peers: self.peers.clone(),
            raw: Bytes::from(raw),
            txid: prepared.txid,
        };
        // cancelled = answer dropped: the caller reads "submission ended"
        tokio::spawn(self.shutdown.clone().run_until_cancelled_owned(job.run(
            self.policy,
            checked,
            answer,
        )));
        answered.await.unwrap_or_else(|_| {
            let gone = NonDomainError::new(FailureMode::Connection, "submission ended");
            Err(SubmitError::Unreachable { attempted: 0, cause: gone })
        })
    }

    /// `checkadmission` on a random trusted validator holding the verified best, the next on no
    /// verdict: `Ok(true)` admissible, `Ok(false)` none (none holds it, supports it or answered)
    ///
    /// - a holder only: one behind the tip refuses what spends a fresh output
    /// - nothing relayed: the check alone is no broadcast
    async fn check(&self, raw: &[u8]) -> Result<bool, SendRawTransactionError> {
        let holders = self.core.current().held_by();
        let mut members = self.balancer.entries();
        members.retain(|member| holders.contains(*member));
        fastrand::shuffle(&mut members);
        for member in members {
            match self.balancer.check_admission(member, raw.to_vec()).await {
                Ok(_) => return Ok(true),
                Err(unanswered) => match unanswered.last {
                    Some(QueryError::Domain(CheckAdmissionError::Refused(refused))) => {
                        return Err(refused)
                    }
                    Some(QueryError::Domain(CheckAdmissionError::Unsupported)) | None => {}
                    Some(QueryError::NonDomain(cause)) => {
                        debug!(?member, %cause, "Admission check unanswered")
                    }
                },
            }
        }
        Ok(false)
    }
}

/// Refusals the bytes and the verified best decide alone (no push: a peer would drop it
/// silently), against the block above the best (`admission.rs`)
///
/// - nothing verified yet = nothing to judge against: pushed as is
/// - code `-25` = zebrad's own for both rules (consensus-invalid)
fn precheck(
    view: &ChainViewSnapshot,
    schedule: &LocalNetwork,
    prepared: &Prepared,
) -> Result<(), SendRawTransactionError> {
    let Some(best) = view.best() else { return Ok(()) };
    valid_in(prepared, NextBlock::above(best.height, schedule)).map_err(|refused| {
        SendRawTransactionError::Rejected { code: -25, message: refused.to_string() }
    })
}

/// What the wallet hears back from one submission
type SubmitAnswer = Result<TxId, SubmitError>;

/// One running submission (spawned: it outlives the wallet's answer)
struct Submission<S> {
    core: Arc<ChainViewCore>,
    balancer: TrafficBalancer<S>,
    peers: Option<Arc<dyn PeerSource>>,
    raw: Bytes,
    txid: TxId,
}

impl<S: ChainDataSource> Submission<S> {
    async fn run(self, policy: SubmitPolicy, checked: bool, answer: oneshot::Sender<SubmitAnswer>) {
        let mut answer = Some(answer);
        let mut published = self.core.subscribe_published();
        let peers = self.peers.as_ref().is_some_and(|peers| !peers.live().is_empty());
        let trusted = self.balancer.entries();
        let mut job = Job::new(peers, trusted, checked, policy, fastrand::Rng::new());
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
                    let unadmitted = "no trusted validator answered and no peer quorum listed it";
                    NonDomainError::new(FailureMode::Connection, unadmitted)
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
            (Entry::Peers, Some(peers)) => match peers.push(self.raw.clone()).await {
                Ok(()) => Pushed::Delivered,
                Err(cause) => Pushed::Unreachable(cause),
            },
            (Entry::Peers, None) => {
                unreachable!("peer relays come only from a PeerSource")
            }
        }
    }

    /// The transaction as the view shows it, against the entries tried so far
    fn seen(&self, tried: &[Entry]) -> Seen {
        let view = self.core.current();
        let sighting = view.sighting(&self.txid);
        let listing = sighting.map(|s| s.trusted()).unwrap_or_default();
        let quorum = sighting.is_some_and(|s| s.quorum());
        Seen::of(listing, quorum, view.mempool_readers(), view.peers_quorate(), tried)
    }
}

/// Cheap-to-clone reader of a running view (no polling, no relay: both on [`ChainView`])
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
    /// Latest published view (one atomic load: pin once per request or stream, not per answer)
    pub fn current(&self) -> Arc<ChainViewSnapshot> {
        self.core.current()
    }

    /// Every publish (one per merge): [`current`](Self::current) has moved
    pub fn subscribe_published(&self) -> tokio::sync::watch::Receiver<()> {
        self.core.subscribe_published()
    }
}
