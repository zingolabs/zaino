//! The aggregate: the write handle that fans a broadcast out, and the read handles.

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;

use tracing::warn;
use zaino_primitives::types::{ReorgDepth, TransactionId};
use zaino_source::{NonDomainError, QueryError, SendRawTransactionError};

use crate::endpoint::EndpointPoller;
use crate::endpoints::{EndpointIndex, ValidatorMetadata};
use crate::error::{BelowQuorum, BroadcastError, ConfigError};
use crate::fold::ChainViewCore;
use crate::ports::EndpointSource;
use crate::quorum::{Quorum, QuorumTip};
use crate::snapshot::{ChainViewSnapshot, MempoolEntry, MempoolView};

/// One operator-configured validator (membership is never discovered: a quorum over a
/// discovered set is not a quorum)
pub struct Endpoint<S: EndpointSource> {
    /// For logs only
    pub address: String,
    pub source: Arc<S>,
}

/// One view over N validators.
///
/// The *write* side: it owns the broadcast fan-out, which is the only operation that mutates
/// the chain. Everything else holds a [`ChainViewSubscriber`].
pub struct ChainView<S: EndpointSource> {
    core: Arc<ChainViewCore>,
    sources: Vec<Arc<S>>,
}

impl<S: EndpointSource> std::fmt::Debug for ChainView<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainView").field("core", &self.core).finish_non_exhaustive()
    }
}

impl<S: EndpointSource> ChainView<S> {
    /// The view + one runnable poller per endpoint (index = position in `endpoints`)
    ///
    /// - `depth` = each endpoint's ancestry window (sync's `finalised_depth`: split deeper than
    ///   the non-final span = no common block → below quorum)
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
            Quorum::over(configured),
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
                depth,
            ));
        }

        Ok((Self { core, sources }, pollers))
    }

    /// A reader onto the same published cell and action channel.
    pub fn subscriber(&self) -> ChainViewSubscriber {
        ChainViewSubscriber { core: Arc::clone(&self.core) }
    }

    /// Relay a transaction to **every** endpoint (§5).
    ///
    /// - any accept ⇒ success, and the transaction is marked `ours`
    /// - mixed accept/reject ⇒ success (a rejecting node usually has a stricter local fee
    ///   filter, not a different idea of validity)
    /// - unanimous domain rejection ⇒ [`BroadcastError::Rejected`], the real one
    ///
    /// Fanning out is unambiguously right here: N entry points propagate faster than one, and
    /// one dead node cannot block a send.
    pub async fn broadcast(&self, raw: Vec<u8>) -> Result<TransactionId, BroadcastError> {
        let attempts = futures::future::join_all(
            self.sources.iter().map(|source| source.send_raw_transaction(raw.clone())),
        )
        .await;

        let mut rejection: Option<SendRawTransactionError> = None;
        let mut unreachable: Option<NonDomainError> = None;
        let mut accepted: Option<TransactionId> = None;

        for attempt in attempts {
            match attempt {
                Ok(txid) => accepted = accepted.or(Some(txid)),
                Err(QueryError::Domain(rejected)) => rejection = rejection.or(Some(rejected)),
                Err(QueryError::NonDomain(cause)) => unreachable = Some(cause),
            }
        }

        match (accepted, unreachable, rejection) {
            (Some(txid), _, rejected) => {
                if let Some(rejected) = rejected {
                    warn!(%txid, %rejected, "Transaction rejected by one validator, accepted by another");
                }
                self.core.mark_ours(txid, Bytes::from(raw));
                Ok(txid)
            }
            (None, Some(cause), _) => {
                Err(BroadcastError::Unreachable { attempted: self.sources.len(), cause })
            }
            (None, None, Some(rejected)) => Err(BroadcastError::Rejected(rejected)),
            // Unreachable: `sources` non-empty, so every endpoint answered one way or the other
            (None, None, None) => Err(BroadcastError::Unreachable {
                attempted: self.sources.len(),
                cause: NonDomainError::new(
                    zaino_source::FailureMode::Connection,
                    "no endpoint answered the broadcast",
                ),
            }),
        }
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

    pub fn quorum(&self) -> Quorum {
        self.core.quorum()
    }

    /// Latest quorum tip, level-triggered (`None` = below quorum): what block sync follows
    pub fn subscribe_tip(&self) -> tokio::sync::watch::Receiver<Option<QuorumTip>> {
        self.core.subscribe_tip()
    }

    /// A snapshot-then-tail feed, backing `GetMempoolStream` (below quorum = the refusal)
    ///
    /// - wake subscribed **before** the anchor is pinned (a fold landing between = one spurious
    ///   wake, never a missed arrival)
    pub fn tail(&self) -> Result<MempoolTail, BelowQuorum> {
        let wake = self.core.subscribe_tails();
        let anchor = self.core.current();
        anchor.mempool()?;

        Ok(MempoolTail {
            core: Arc::clone(&self.core),
            wake,
            walked: anchor.arrivals().len(),
            anchor,
            delivered: HashSet::new(),
            pending: VecDeque::new(),
            closed: false,
        })
    }
}

/// One client's `GetMempoolStream`: [`snapshot`](Self::snapshot), then
/// [`next`](Self::next) until a block ends it.
///
/// # Snapshot-then-tail, and when it ends
///
/// A subscriber gets the whole servable mempool first, then the tail. Tail-only would be wrong
/// for the one caller that exists: pepper-sync restarts this stream in a loop and has no other
/// way to learn about a transaction that arrived while it was reconnecting.
///
/// The stream ends when a **new block is mined** (`service.proto:308-309`, here the *quorum* tip
/// moving), not when the mempool empties. An empty mempool with no block mined is a live,
/// silent stream; lightwalletd does the same.
///
/// Losing quorum ends it too (fail closed). Either way the client reconnects.
///
/// # Cost per subscriber
///
/// - Shared: the anchor snapshot and the epoch's arrivals log (`imbl`, `O(1)` to pin)
/// - Own: the txids delivered after the snapshot
/// - Woken only by an arrival or a tip move, never by propagation churn
pub struct MempoolTail {
    core: Arc<ChainViewCore>,
    wake: tokio::sync::watch::Receiver<()>,
    /// View the snapshot was taken from (its epoch bounds the stream)
    anchor: Arc<ChainViewSnapshot>,
    /// Prefix of the epoch's arrivals already considered
    walked: usize,
    delivered: HashSet<TransactionId>,
    pending: VecDeque<MempoolEntry>,
    closed: bool,
}

impl std::fmt::Debug for MempoolTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MempoolTail")
            .field("anchor", &self.anchor.tip())
            .field("walked", &self.walked)
            .field("pending", &self.pending.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl MempoolTail {
    /// The servable mempool the stream opens with (checked at [`ChainViewSubscriber::tail`])
    pub fn snapshot(&self) -> MempoolView<'_> {
        MempoolView::of(&self.anchor)
    }

    /// Identity of [`snapshot`](Self::snapshot): one published view = one answer, so a
    /// serving layer may render it once and share it across every tail anchored there
    pub fn anchor(&self) -> &Arc<ChainViewSnapshot> {
        &self.anchor
    }

    /// Next transaction turned servable after the snapshot, or `None` once the stream ended
    ///
    /// - cancel-safe: state moves only after the wake, and the wake is level-triggered
    pub async fn next(&mut self) -> Option<MempoolEntry> {
        loop {
            if let Some(entry) = self.pending.pop_front() {
                return Some(entry);
            }
            if self.closed {
                return None;
            }
            // Sender lives in `core`, which this tail holds: `Err` is unreachable
            if self.wake.changed().await.is_err() {
                self.closed = true;
                continue;
            }
            self.walk(self.core.current());
        }
    }

    /// Queues arrivals past `walked` that the snapshot or an earlier `next` did not carry
    fn walk(&mut self, pinned: Arc<ChainViewSnapshot>) {
        if pinned.epoch() != self.anchor.epoch() {
            self.closed = true;
            return;
        }
        let (Ok(now), anchored) = (pinned.mempool(), MempoolView::of(&self.anchor)) else {
            self.closed = true;
            return;
        };
        let fresh = pinned.arrivals().iter().skip(self.walked);
        for txid in fresh {
            if anchored.serves(txid) || self.delivered.contains(txid) {
                continue;
            }
            // Flapped out since: its next crossing re-appends it
            if let Some(entry) = now.get(txid) {
                self.delivered.insert(*txid);
                self.pending.push_back(entry);
            }
        }
        self.walked = pinned.arrivals().len();
    }
}
