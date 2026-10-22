//! [`ObservationMerge`]: each trusted validator's polls (the balancer's [`Observation`]s) →
//! `TrustedValidatorReport`s merged into the view
//!
//! - Diff = its listing vs its own last one (`O(change)` per poll, never a whole mempool)
//! - Reading (claim, `getblockhash`) merged first, then the listing (holder facts never wait on
//!   bytes)
//! - Bytes = only what the view lacks, through `bytes(..)` with this validator preferred
//! - Transaction leaves a validator's listing one way: unlisted (mined or evicted)
//! - New tip clears nothing (an unmined transaction survives a block)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zaino_primitives::types::BlockRef;
use zaino_source::{
    BlockchainInfo, ChainDataSource, GetAtHeightError, GetRawMempoolTransactionError,
    MempoolListed, MetadataReading, PollReading, QueryError, Telemetry,
};
use zaino_traffic::{Health, MemberId, Observation, TrafficBalancer, TrustedValidatorId};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{BlockHash, TxId};

use crate::chain_view_core::ChainViewCore;

/// Merges every trusted validator's observations into the view (spawn [`run`](Self::run))
pub struct ObservationMerge<S> {
    pub(crate) core: Arc<ChainViewCore>,
    pub(crate) balancer: TrafficBalancer<S>,
}

impl<S: ChainDataSource> ObservationMerge<S> {
    /// Until `cancel`: each validator's latest poll, as it lands (validators independent)
    pub async fn run(self, cancel: CancellationToken) {
        let addresses = self.balancer.addresses().iter().enumerate();
        let followers = addresses.map(|(index, address)| {
            let validator =
                TrustedValidatorId::new(index).expect("configured below TrustedValidatorId::MAX");
            let merging = ValidatorFollower {
                validator,
                address,
                core: &self.core,
                balancer: &self.balancer,
                listed: BTreeSet::new(),
            };
            merging.follow()
        });
        cancel.run_until_cancelled(join_all(followers)).await;
    }
}

/// One txid a member listed
///
/// - `raw` = `None` when the view already held it (bytes fetched once, never per member)
#[derive(Debug, Clone)]
pub(crate) struct Sighted {
    pub(crate) txid: TxId,
    pub(crate) raw: Option<Bytes>,
    pub(crate) fee: Zatoshis,
}

/// What every answered poll read, mempool on or off
///
/// - `block_at_best` = its `getblockhash` answer at the best the poll started under
/// - `telemetry` = this poll's answered reads (absorbed over the last)
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) block_at_best: Option<BlockRef>,
    pub(crate) info: BlockchainInfo,
    pub(crate) telemetry: Telemetry,
    pub(crate) streaming: bool,
}

/// One trusted validator's poll-to-poll mempool change
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) added: Vec<Sighted>,
    pub(crate) removed: Vec<TxId>,
}

/// One member's poll, as the merge takes it: its reading first, then its listing (bytes fetched
/// between: holder facts never wait on them)
#[derive(Debug, Clone)]
pub(crate) enum TrustedValidatorReport {
    /// `listed` = mempool read (its listing follows), else sightings retracted
    Read {
        reading: Reading,
        health: Health,
        listed: bool,
    },
    Listed(Listing),
    /// `Down` = neither chain nor sightings, else sightings kept + holds no tip
    Failed(Health),
}

/// - `listed` = its last listing as admitted (the diff's left side)
struct ValidatorFollower<'a, S> {
    validator: TrustedValidatorId,
    address: &'a str,
    core: &'a ChainViewCore,
    balancer: &'a TrafficBalancer<S>,
    listed: BTreeSet<TxId>,
}

impl<S: ChainDataSource> ValidatorFollower<'_, S> {
    async fn follow(mut self) {
        let mut observations = self.balancer.observe(self.validator);
        // a poll landed before this subscribed: merged too
        observations.mark_changed();
        while observations.changed().await.is_ok() {
            let observation = observations.borrow_and_update().clone();
            if let Some(observation) = observation {
                self.merge(&observation).await;
            }
        }
    }

    async fn merge(&mut self, observation: &Observation) {
        let polled = match &observation.polled {
            Ok(polled) => polled,
            Err(_) => return self.failed(observation.health),
        };
        let PollReading { info, listing, held, metadata } = polled;
        let telemetry = self.telemetry(metadata.as_ref());
        let held = self.held(observation.asked, held);
        let streaming = observation.streaming;
        let reading = Reading { block_at_best: held, info: info.clone(), telemetry, streaming };
        let health = observation.health;
        match listing {
            Ok(listing) => {
                let report = TrustedValidatorReport::Read { reading, health, listed: true };
                self.core.apply(self.validator, report);
                self.listed(listing).await;
            }
            Err(_) => {
                let report = TrustedValidatorReport::Read { reading, health, listed: false };
                self.core.apply(self.validator, report);
                self.listed.clear();
            }
        }
    }

    /// Mempool read (its reading already merged): the diff, bytes for what the view lacks, one
    /// report
    async fn listed(&mut self, listing: &[MempoolListed]) {
        let listing: BTreeMap<TxId, MempoolListed> =
            listing.iter().map(|entry| (entry.txid, *entry)).collect();
        let removed: Vec<TxId> =
            self.listed.iter().filter(|txid| !listing.contains_key(txid)).copied().collect();
        let mut admitted: BTreeSet<TxId> =
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
        for txid in self.core.apply(self.validator, TrustedValidatorReport::Listed(listing)) {
            admitted.remove(&txid);
        }
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
        let prefer = vec![MemberId::TrustedValidator(self.validator)];
        match self.balancer.bytes(unheld.to_vec(), prefer).await {
            Ok(answered) => answered.value,
            Err(unanswered) => {
                debug!(validator = self.address, %unanswered, "Mempool bytes unanswered");
                Vec::new()
            }
        }
    }

    /// - `Degraded`: holds no tip, sightings kept
    /// - `Down`: neither (its next answer re-lists all)
    fn failed(&mut self, health: Health) {
        self.core.apply(self.validator, TrustedValidatorReport::Failed(health));
        if health == Health::Down {
            self.listed.clear();
        }
    }

    /// Its best-chain block at the `asked` height (above its tip or the item failed = no fact)
    fn held(
        &self,
        asked: Option<BlockHeight>,
        held: &[Result<BlockHash, QueryError<GetAtHeightError>>],
    ) -> Option<BlockRef> {
        match (asked?, held.first()?) {
            (height, Ok(hash)) => Some(BlockRef { hash: *hash, height }),
            (_, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(_)))) => None,
            (height, Err(cause @ (QueryError::NonDomain(_) | QueryError::Unsupported))) => {
                debug!(validator = self.address, ?height, %cause, "getblockhash unanswered");
                None
            }
        }
    }

    /// A metadata poll's answered telemetry, each failed read warned (its last answer kept)
    fn telemetry(&self, metadata: Option<&MetadataReading>) -> Telemetry {
        let Some(MetadataReading { answered, failed }) = metadata else {
            return Telemetry::default();
        };
        for (what, cause) in failed {
            warn!(validator = self.address, %cause, "Validator {what} read failed, last kept");
        }
        answered.clone()
    }
}
