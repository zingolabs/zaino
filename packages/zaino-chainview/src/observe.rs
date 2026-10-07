//! Each trusted member's polls (the balancer's [`Observation`]s) folded into the view
//!
//! - Diff = its listing vs its own last one (`O(change)` per poll, never a whole mempool)
//! - Bytes = only what the view lacks, through `bytes(..)` with this member preferred (§5)
//! - A transaction leaves a member's listing one way: unlisted (mined or evicted); a new tip
//!   clears nothing (an unmined transaction survives a block)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use futures::future::join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_primitives::types::{BlockHash, BlockRef, Height, TransactionId};
use zaino_source::{
    ChainDataSource, GetAtHeightError, GetRawMempoolTransactionError, MempoolListed,
    MetadataReading, NonDomainError, PollReading, QueryError,
};
use zaino_traffic::{Health, MemberId, Observation, TrafficBalancer, ValidatorId};

use crate::config::CATCHING_UP_WARN_INTERVAL;
use crate::fold::{ChainViewCore, EndpointReport, Listing, Reading, Sighted};

/// Folds every trusted member's observations into the view; spawn [`run`](Self::run)
pub struct ObservationFold<S> {
    pub(crate) core: Arc<ChainViewCore>,
    pub(crate) balancer: TrafficBalancer<S>,
    pub(crate) addresses: Vec<String>,
}

impl<S: ChainDataSource> ObservationFold<S> {
    /// Until `cancel`: each member's latest poll, as it lands (members independent)
    pub async fn run(self, cancel: CancellationToken) {
        let members = self.addresses.iter().enumerate().map(|(index, address)| {
            let member = ValidatorId::new(index).expect("configured below ValidatorId::MAX");
            let folding = Member {
                member,
                address,
                core: &self.core,
                balancer: &self.balancer,
                listed: BTreeSet::new(),
                health: Health::Pending,
                catching_up_warned: None,
            };
            folding.follow()
        });
        cancel.run_until_cancelled(join_all(members)).await;
    }
}

/// `listed` = its last listing as admitted (the diff's left side); `health` = as last folded
struct Member<'a, S> {
    member: ValidatorId,
    address: &'a str,
    core: &'a ChainViewCore,
    balancer: &'a TrafficBalancer<S>,
    listed: BTreeSet<TransactionId>,
    health: Health,
    catching_up_warned: Option<Instant>,
}

impl<S: ChainDataSource> Member<'_, S> {
    async fn follow(mut self) {
        let mut observations = self.balancer.observe(self.member);
        // a poll landed before this subscribed: folded too
        observations.mark_changed();
        while observations.changed().await.is_ok() {
            let observation = observations.borrow_and_update().clone();
            if let Some(observation) = observation {
                self.fold(&observation).await;
            }
        }
    }

    async fn fold(&mut self, observation: &Observation) {
        let polled = match &observation.polled {
            Ok(polled) => polled,
            Err(cause) => return self.failed(cause, observation.health),
        };
        let PollReading { info, listing, held, metadata } = polled;
        let (peers, release) = self.metadata(metadata.as_ref());
        let held = self.held(&observation.asked, held);
        let streaming = observation.streaming;
        let reading = Reading { held, info: info.clone(), peers, release, streaming };
        match listing {
            Ok(listing) => self.listed(reading, listing).await,
            Err(_) => {
                self.catching_up(&reading);
                self.core.apply(self.member, EndpointReport::CatchingUp(reading));
                self.listed.clear();
            }
        }
    }

    /// Mempool read: the diff, bytes for what the view lacks, one report
    async fn listed(&mut self, reading: Reading, listing: &[MempoolListed]) {
        let listing: BTreeMap<TransactionId, MempoolListed> =
            listing.iter().map(|entry| (entry.txid, *entry)).collect();
        let removed: Vec<TransactionId> =
            self.listed.iter().filter(|txid| !listing.contains_key(txid)).copied().collect();
        let mut admitted: BTreeSet<TransactionId> =
            listing.keys().filter(|txid| self.listed.contains(txid)).copied().collect();
        let mut added = Vec::new();
        let mut unheld = Vec::new();
        for entry in listing.values().filter(|entry| !self.listed.contains(&entry.txid)) {
            match self.core.holds(&entry.txid) {
                true => {
                    added.push(Sighted { txid: entry.txid, raw: None, fee: entry.fee });
                    admitted.insert(entry.txid);
                }
                false => unheld.push(*entry),
            }
        }
        for (entry, raw) in unheld.iter().zip(self.bytes(&unheld).await) {
            match raw {
                Ok(raw) => {
                    let raw = Some(Bytes::from(raw));
                    added.push(Sighted { txid: entry.txid, raw, fee: entry.fee });
                    admitted.insert(entry.txid);
                }
                // listed, then mined or evicted before the fetch
                Err(GetRawMempoolTransactionError::NotFound(txid)) => {
                    debug!(%txid, "Mempool transaction gone before fetch")
                }
            }
        }
        let listing = Listing { added, removed };
        for txid in self.core.apply(self.member, EndpointReport::Observed(reading, listing)) {
            admitted.remove(&txid);
        }
        self.live(admitted.len());
        self.listed = admitted;
    }

    /// Bytes of `unheld` (unanswered = none: each re-listed, re-fetched next poll)
    async fn bytes(
        &self,
        unheld: &[MempoolListed],
    ) -> Vec<Result<Vec<u8>, GetRawMempoolTransactionError>> {
        if unheld.is_empty() {
            return Vec::new();
        }
        let prefer = vec![MemberId::Trusted(self.member)];
        match self.balancer.bytes(unheld.to_vec(), prefer).await {
            Ok(answered) => answered.value,
            Err(unanswered) => {
                debug!(endpoint = self.address, %unanswered, "Mempool bytes unanswered");
                Vec::new()
            }
        }
    }

    /// `Degraded`: holds no tip, sightings kept; `Down`: neither (its next answer re-lists all)
    fn failed(&mut self, cause: &NonDomainError, health: Health) {
        match health {
            Health::Down => {
                if self.health != Health::Down {
                    warn!(endpoint = self.address, %cause, "Validator down, holds no tip");
                }
                self.core.apply(self.member, EndpointReport::Down);
                self.listed.clear();
            }
            _ => {
                warn!(endpoint = self.address, %cause, "Validator poll failed");
                self.core.apply(self.member, EndpointReport::Failed);
            }
        }
        self.health = health;
    }

    fn live(&mut self, listed: usize) {
        match std::mem::replace(&mut self.health, Health::Live) {
            Health::Live => {}
            Health::CatchingUp => info!(endpoint = self.address, listed, "Validator caught up"),
            Health::Pending => info!(endpoint = self.address, listed, "Polling validator"),
            Health::Degraded | Health::Down => info!(endpoint = self.address, "Validator back"),
        }
        self.catching_up_warned = None;
    }

    /// Warned once per [`CATCHING_UP_WARN_INTERVAL`]
    fn catching_up(&mut self, reading: &Reading) {
        self.health = Health::CatchingUp;
        let due =
            self.catching_up_warned.is_none_or(|at| at.elapsed() >= CATCHING_UP_WARN_INTERVAL);
        if due {
            let (tip, network) = (reading.info.blocks, reading.info.estimated_height);
            let behind = u32::from(network).saturating_sub(tip.into());
            let height = u32::from(tip);
            let hash = reading.info.best_block_hash;
            warn!(endpoint = self.address, height, behind, %hash, "Validator catching up");
            self.catching_up_warned = Some(Instant::now());
        }
    }

    /// Its best-chain blocks at the `asked` heights (above its tip or an item failed = no fact)
    fn held(
        &self,
        asked: &[Height],
        held: &[Result<BlockHash, QueryError<GetAtHeightError>>],
    ) -> Vec<BlockRef> {
        let answers = asked.iter().zip(held).filter_map(|(height, answer)| match answer {
            Ok(hash) => Some(BlockRef { hash: *hash, height: *height }),
            Err(QueryError::Domain(GetAtHeightError::HeightNotFound(_))) => None,
            Err(QueryError::NonDomain(cause)) => {
                debug!(endpoint = self.address, ?height, %cause, "getblockhash unanswered");
                None
            }
        });
        answers.collect()
    }

    /// Telemetry halves of a metadata poll: a failed half keeps the last answer (`None`), warned
    fn metadata(
        &self,
        metadata: Option<&MetadataReading>,
    ) -> (
        Option<Vec<zaino_primitives::types::PeerInfo>>,
        Option<zaino_primitives::types::NodeRelease>,
    ) {
        let Some(MetadataReading { peers, release }) = metadata else { return (None, None) };
        let kept = |what: &str, cause: &NonDomainError| warn!(endpoint = self.address, %cause, "Validator {what} read failed, last kept");
        let peers = peers.as_ref().inspect_err(|cause| kept("peer list", cause)).ok().cloned();
        let release = release.as_ref().inspect_err(|cause| kept("release", cause)).ok().cloned();
        (peers, release)
    }
}
