//! [`validate`] (precheck + `checkadmission`, nothing relayed), then [`broadcast`] (a spawned
//! `Submission` executing `Job`'s steps)

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zaino_source::{
    prepare_transaction, ChainDataSource, CheckAdmissionError, FailureMode, NonDomainError,
    Prepared, QueryError, SendRawTransactionError,
};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::TxId;

use super::policy::{Ended, Entry, Job, Pushed, Seen, Step, SubmitPolicy};
use crate::admission::{valid_in, NextBlock};
use crate::chain_view_core::ChainViewCore;
use crate::chain_view_snapshot::ChainViewSnapshot;
use crate::error::SubmitError;
use crate::ports::PeerSource;
use crate::telemetry;

/// Bytes [`validate`] passed: the only input [`broadcast`] takes (no relay without the checks)
///
/// - `admitted` = a holder's `checkadmission` said yes (`false` = none could judge it)
#[derive(Debug)]
pub struct ValidatedTransaction {
    raw: Vec<u8>,
    txid: TxId,
    admitted: bool,
}

/// Decoded, prechecked against the block above best, then one holder's `checkadmission`
/// (nothing relayed: a refusal never leaves Zaino)
pub(crate) async fn validate<S: ChainDataSource>(
    core: &ChainViewCore,
    balancer: &TrafficBalancer<S>,
    schedule: &LocalNetwork,
    raw: Vec<u8>,
) -> Result<ValidatedTransaction, SubmitError> {
    let prepared = prepare_transaction(&raw).map_err(|malformed| {
        SubmitError::Rejected(SendRawTransactionError::Malformed(malformed.to_string()))
    })?;
    precheck(&core.current(), schedule, &prepared).map_err(SubmitError::Rejected)?;
    let admitted = check(core, balancer, &raw).await.map_err(|refused| {
        telemetry::submitted(&Ended::Rejected(refused.clone()), 0);
        SubmitError::Rejected(refused)
    })?;
    Ok(ValidatedTransaction { raw, txid: prepared.txid, admitted })
}

/// Relay job for `validated` → its txid on the first acceptance, else the verdict
///
/// - job outlives the answer (`shutdown` cancels it)
/// - protocol: `docs/architecture.md` §6, API: `usage.md` "Submission"
pub(crate) async fn broadcast<S: ChainDataSource>(
    core: &Arc<ChainViewCore>,
    balancer: &TrafficBalancer<S>,
    peers: &Option<Arc<dyn PeerSource>>,
    policy: SubmitPolicy,
    shutdown: &CancellationToken,
    validated: ValidatedTransaction,
) -> Result<TxId, SubmitError> {
    let ValidatedTransaction { raw, txid, admitted } = validated;
    let (answer, answered) = oneshot::channel();
    let job = Submission {
        core: Arc::clone(core),
        balancer: balancer.clone(),
        peers: peers.clone(),
        raw: Bytes::from(raw),
        txid,
    };
    // cancelled = answer dropped: the caller reads "submission ended"
    tokio::spawn(shutdown.clone().run_until_cancelled_owned(job.run(policy, admitted, answer)));
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
async fn check<S: ChainDataSource>(
    core: &ChainViewCore,
    balancer: &TrafficBalancer<S>,
    raw: &[u8],
) -> Result<bool, SendRawTransactionError> {
    let holders = core.current().held_by();
    let mut members = balancer.entries();
    members.retain(|member| holders.contains(*member));
    fastrand::shuffle(&mut members);
    for member in members {
        match balancer.check_admission(member, raw.to_vec()).await {
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
    async fn run(
        self,
        policy: SubmitPolicy,
        admitted: bool,
        answer: oneshot::Sender<SubmitAnswer>,
    ) {
        let mut answer = Some(answer);
        let mut published = self.core.subscribe_published();
        let peers = self.peers.as_ref().is_some_and(|peers| !peers.live().is_empty());
        let trusted = self.balancer.entries();
        let mut job = Job::new(peers, trusted, admitted, policy, fastrand::Rng::new());
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
