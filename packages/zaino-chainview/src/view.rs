//! [`ChainView`] (submitting handle + runnables) + [`ChainViewSubscriber`] (read handle)

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::FinalityDepth;
use zaino_source::ChainDataSource;
use zaino_traffic::TrafficBalancer;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::TxId;

use crate::chain_view_core::ChainViewCore;
use crate::chain_view_snapshot::ChainViewSnapshot;
use crate::error::SubmitError;
use crate::header_sync::HeaderSync;
use crate::observation_merge::ObservationMerge;
use crate::peer_mempool::PeerMempool;
use crate::peers::PeerWatch;
use crate::ports::{PeerSource, UnspentOutputs};
use crate::submit::{self, SubmitPolicy, ValidatedTransaction};
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

    /// Decode + precheck + one holder's `checkadmission`, nothing relayed
    pub async fn validate_transaction(
        &self,
        raw: Vec<u8>,
    ) -> Result<ValidatedTransaction, SubmitError> {
        submit::validate(&self.core, &self.balancer, &self.schedule, raw).await
    }

    /// Relay to the validators (+ peers) → txid on the first acceptance (relay outlives the answer)
    /// - protocol: `docs/architecture.md` §6, API: `usage.md` "Submission"
    pub async fn broadcast_transaction(
        &self,
        validated: ValidatedTransaction,
    ) -> Result<TxId, SubmitError> {
        let (core, balancer, peers) = (&self.core, &self.balancer, &self.peers);
        submit::broadcast(core, balancer, peers, self.policy, &self.shutdown, validated).await
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
