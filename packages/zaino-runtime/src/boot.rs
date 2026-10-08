//! Booting a deployment: the indexed assembly.
//!
//! [`boot_indexed`] brings up every deployment that builds a local finalised
//! index — the LMDB-backed store and its indexer, the self-synchronising
//! non-finalised chain head, the engine that composes the two with the
//! validator under the deployment's routing, and the serving adapter the
//! caller supplies — all supervised under one [`Orchestra`], the validator
//! gated first.
//!
//! Generic over the deployment: the namespaces the backend opens, the set the
//! indexer builds, the type the store reader is wired to and the routing the
//! engine composes under all come from `D`, so none of them can be paired
//! wrongly here. `demand ⊆ supply` is checked once, at [`compose`].

use std::num::NonZeroU32;
use std::sync::Arc;

use tracing::{info, warn};

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_head_service::{ChainHeadInitError, ChainHeadService, ChainHeadSubscriber};
use zaino_component::{ComponentName, Managed, ReachabilityProbe, StatusSource, StatusWatch};
use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_core::chain_view::ChainTier;
use zaino_indexer::{SourceFetch, SourceSyncDriver, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::current_zaino::{
    context_from_block, context_from_pre_index_compact_block, CurrentZainoContext,
};
use zaino_persistence::{NamespaceSpec, OpenError};
use zaino_persistence_codec::reserved_namespaces;
use zaino_primitives::types::PoolActivations;
use zaino_service::use_cases::{Serves, UseCase};
use zaino_service::TakeSnapshot;
use zaino_source::GetBlockchainInfo;
use zaino_store::{IndexCoverageError, StoreReader, WatermarkRepairError};
use zaino_store_service::StoreComponent;

use crate::config::{FetchStrategy, IndexedDeploymentConfig};
use crate::deployment::{compose, DeploymentEngine, IndexedSource};
use crate::orchestra::{Orchestra, OrchestraBuilder};
use crate::plan::RuntimePlan;
use crate::run_component::RunComponent;
use crate::validator::{ValidatorComponent, ValidatorUnreachable};

/// The engine [`boot_indexed`] wires for deployment `D` over the validator
/// client `C`: the LMDB store over the deployment's index set, the chain
/// head, and the client, under the deployment's routing.
///
/// The validator is the second supply axis beside the index set: a validator
/// lacking a port the deployment's routing sends to it fails at the same
/// `compose` bound a missing index does.
pub type IndexedEngine<D, C> = DeploymentEngine<D, LmdbBackend, ChainHeadSubscriber, C>;

/// The indexed assembly could not be brought up.
///
/// Each variant keeps its cause typed; only the component-boot boundary is
/// boxed, since [`OrchestraBuilder::boot`] is generic over each component's
/// error type and one enum cannot name them all.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    /// Opening the LMDB index store failed.
    #[error("opening the index store failed")]
    OpenStore(#[source] OpenError),
    /// The store's watermark could not be checked against, or corrected to,
    /// what the headers index actually holds.
    #[error("checking the store's watermark against its index failed")]
    StoreWatermark(#[source] WatermarkRepairError),
    /// The deployment declares an index the existing store never built, so
    /// opening it would serve that index's reads as complete while they cover
    /// only part of the chain. The runtime refuses to boot; the error names the
    /// unstamped index(es) and the remedy.
    #[error(transparent)]
    IndexCoverage(#[from] IndexCoverageError),
    /// Building the sync stack (backend, provisioner, engine) failed.
    #[error("building the indexer failed")]
    Indexer(#[source] zaino_indexer::IndexerError),
    /// The non-finalised chain head could not anchor against the validator.
    #[error("the chain head could not anchor against the validator")]
    ChainHeadInit(#[source] ChainHeadInitError),
    /// The validator was unreachable when the runtime gated on it at boot.
    #[error(transparent)]
    ValidatorUnreachable(#[from] ValidatorUnreachable),
    /// The validator's upgrade schedule could not be read at boot. The store
    /// needs it to serve treestate correctly (an active-but-empty pool must serve
    /// the empty tree, not be reported absent), and it is read from the validator
    /// rather than compiled in, so the runtime refuses to boot without it rather
    /// than silently defaulting to a wrong schedule.
    #[error("reading the validator's upgrade schedule at boot failed: {0}")]
    ActivationSchedule(String),
    /// A runtime component failed to boot.
    #[error("component failed to boot")]
    Component(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Boot the indexed assembly for deployment `D` over the shared `client`,
/// serving its engine through `serve`.
///
/// Every consumer — the indexer, the chain head, the engine's passthrough —
/// reads through the one client; none holds a raw validator. The caller has
/// already confirmed the validator is reachable (it built the client), so the
/// runtime's validator gate is a formality here.
///
/// Readiness-gated order: validator, then the indexer (so its watermark is
/// published before the chain head trims against it), then the store, then
/// the chain-head writer, then the server. The returned [`Orchestra`]
/// supervises all of them; the caller runs it until a signal or an
/// escalation.
pub async fn boot_indexed<D, A, C>(
    client: Arc<C>,
    config: &D::Config,
    serve: impl FnOnce(IndexedEngine<D, C>) -> A,
) -> Result<Orchestra, DeployError>
where
    D: RuntimePlan<Config = IndexedDeploymentConfig>,
    C: IndexedSource + GetBlockchainInfo,
    StoreReader<LmdbBackend, D::Indexes>: TakeSnapshot<Snapshot: ChainTier>,
    IndexedEngine<D, C>: Serves<D::UseCase>,
    RunComponent<A>: StatusSource + StatusWatch + Managed + Clone + Send + Sync + 'static,
{
    let source = client;

    let backend = open_store::<D>(&config.store)?;

    // The per-pool activation schedule, read once from the validator at boot. The
    // store needs it to serve treestate correctly — an active-but-empty pool
    // (from activation to its first note) serves the empty tree, not absence —
    // and Zaino carries no compiled-in schedule, so a validator that cannot
    // describe its chain fails the boot rather than defaulting to a wrong one.
    let activations = read_pool_activations(source.as_ref()).await?;

    // The finalised store: the indexer writes it, the engine composes blocks on
    // read from it. One reader, shared (Arc-backed clone). Typed to the
    // deployment's index set: the reads it has are exactly the reads those
    // indexes back. It carries the activation schedule so its treestate read
    // renders each pool against the right boundary.
    let store_reader =
        StoreReader::<_, D::Indexes>::with_activations(Arc::new(backend.clone()), activations);
    repair_watermark::<D>(&store_reader)?;
    // Fail loud before the indexer stamps the new indexes on first write: an
    // index this deployment declares but the existing store never built would
    // cover only [resume, tip], and then report serviceable. Checked against
    // the repaired watermark, so a store that has synced nothing still opens.
    store_reader.check_index_coverage()?;

    let tuning = SyncTuning {
        batch_size: config.indexer.batch_size,
        finalised_depth: config.indexer.finalised_depth,
        channel_capacity: config.indexer.channel_capacity,
        concurrency: config.indexer.concurrency,
    };

    // The indexer builds the deployment's index set from what the validator
    // hands it per height, resuming from the backend watermark. Which read it
    // asks for is the configured fetch strategy; both project to the one
    // provisioning context, so the index built is the same either way.
    match config.indexer.fetch {
        FetchStrategy::Compact => {
            let driver = SourceSyncDriver::resuming_compact(
                &backend,
                D::Indexes::pipelines(),
                Arc::clone(&source),
                |compact_block| context_from_pre_index_compact_block(&compact_block),
                tuning,
            )
            .map_err(DeployError::Indexer)?
            .with_deferral(config.store.deferred_writes);
            assemble::<D, A, C, _, _>(source, store_reader, driver, serve).await
        }
        FetchStrategy::Full => {
            let driver = SourceSyncDriver::resuming(
                &backend,
                D::Indexes::pipelines(),
                Arc::clone(&source),
                |block| context_from_block(&block),
                tuning,
            )
            .map_err(DeployError::Indexer)?
            .with_deferral(config.store.deferred_writes);
            assemble::<D, A, C, _, _>(source, store_reader, driver, serve).await
        }
    }
}

/// Read the validator's reported upgrade schedule once and project it to the
/// per-pool activation heights, keyed by consensus branch id.
///
/// A validator that cannot describe its chain fails the boot: Zaino carries no
/// compiled-in schedule, and defaulting to a wrong one would make the treestate
/// read report an active pool absent (or an inactive one present) — which
/// pepper-sync rejects.
async fn read_pool_activations<C: GetBlockchainInfo>(
    source: &C,
) -> Result<PoolActivations, DeployError> {
    let info = source
        .get_blockchain_info()
        .await
        .map_err(|e| DeployError::ActivationSchedule(e.to_string()))?;
    Ok(PoolActivations::from_branch_activations(
        info.upgrades
            .iter()
            .map(|upgrade| (u32::from(upgrade.branch_id), upgrade.activation_height)),
    ))
}

/// Bring up everything around an indexer that is already built: the chain
/// head anchored over the same client, the engine composed under the
/// deployment's routing, the serving adapter, and the Orchestra over the lot.
async fn assemble<D, A, C, F, Fetch>(
    source: Arc<C>,
    store_reader: StoreReader<LmdbBackend, D::Indexes>,
    driver: SourceSyncDriver<C, LmdbBackend, CurrentZainoContext, F, Fetch>,
    serve: impl FnOnce(IndexedEngine<D, C>) -> A,
) -> Result<Orchestra, DeployError>
where
    D: RuntimePlan<Config = IndexedDeploymentConfig>,
    C: IndexedSource,
    StoreReader<LmdbBackend, D::Indexes>: TakeSnapshot<Snapshot: ChainTier>,
    IndexedEngine<D, C>: Serves<D::UseCase>,
    RunComponent<A>: StatusSource + StatusWatch + Managed + Clone + Send + Sync + 'static,
    Fetch: SourceFetch<C>,
    F: Fn(Fetch::Item) -> CurrentZainoContext + Send + Sync + 'static,
{
    // Capture the confirmed-watermark receiver before the driver is moved into
    // its component — it is the chain head's only handle onto what the store
    // has durably committed (confirm-before-trim).
    let confirmed_watermark = driver.subscribe_confirmed_watermark();

    // The chain head, anchored over the same client: it binds the canonical
    // ports, so retrying lives in the client and the chain head carries no
    // ladder of its own.
    let (chain_head_subscriber, chain_head_writer) = ChainHeadService::anchor(
        Arc::clone(&source),
        ChainHeadConfig::with_max_depth(
            NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT).expect("the consensus reorg bound is non-zero"),
        ),
        confirmed_watermark,
    )
    .await
    .map_err(DeployError::ChainHeadInit)?;

    // Compose store ⊕ head ⊕ validator into the served engine under the
    // deployment's routing. The passthrough side consumes the resilient client
    // over the shared validator — the canonical ports, never the raw one-shots,
    // and never the concrete adapter type. That this engine serves what the
    // use case demands is the `compose` bound.
    let engine = compose::<D, _, _, _>(
        store_reader.clone(),
        chain_head_subscriber,
        (*source).clone(),
    );

    let validator_component = ValidatorComponent::connect(&AlreadyReachable).await?;
    let indexer = RunComponent::new(ComponentName("indexer"), driver);
    let store = StoreComponent::new(ComponentName("store"), store_reader);
    // The chain-head writer is escalated and supervised exactly like the indexer.
    let chain_head = RunComponent::new(ComponentName("chain-head"), chain_head_writer);
    let server = RunComponent::new(ComponentName(D::UseCase::NAME), serve(engine));

    let orchestra = OrchestraBuilder::new()
        .with_readiness(D::READINESS)
        .boot_observed(validator_component)
        .await
        .boot(indexer)
        .await
        .map_err(|e| DeployError::Component(Box::new(e)))?
        .boot(store)
        .await
        .map_err(|e| DeployError::Component(Box::new(e)))?
        .boot(chain_head)
        .await
        .map_err(|e| DeployError::Component(Box::new(e)))?
        .boot(server)
        .await
        .map_err(|e| DeployError::Component(Box::new(e)))?
        .build();

    info!(
        use_case = D::UseCase::NAME,
        "runtime booted; serving over the composed store⊕head chain"
    );
    Ok(orchestra)
}

/// Open the LMDB backend with every namespace the deployment writes declared
/// up front: one per index in the set, plus the engine's reserved watermark /
/// format-version namespaces. The set is the deployment's index set — the
/// same type the store reader is wired over, so what is built and what is
/// served cannot drift.
fn open_store<D: RuntimePlan>(
    config: &crate::config::StoreConfig,
) -> Result<LmdbBackend, DeployError> {
    let namespaces: Vec<NamespaceSpec> = D::Indexes::pipelines()
        .namespace_specs()
        .into_iter()
        .chain(reserved_namespaces().map(NamespaceSpec::meta))
        .collect();
    LmdbBackend::open(LmdbConfig {
        path: config.path.clone(),
        map_size_bytes: config.map_size_gb << 30,
        namespaces,
    })
    .map_err(DeployError::OpenStore)
}

/// A watermark ahead of the headers index claims heights the store cannot
/// serve, and every read in that gap would be routed to it and answer nothing.
/// Data outranks the stamp: correct it to the highest header held before the
/// indexer resumes from it or the engine serves against it. Loud when it fires.
fn repair_watermark<D: RuntimePlan>(
    store_reader: &StoreReader<LmdbBackend, D::Indexes>,
) -> Result<(), DeployError> {
    if let Some(repair) = store_reader
        .repair_watermark()
        .map_err(DeployError::StoreWatermark)?
    {
        // The per-namespace trim counts: which walk-ordered indexes held
        // heights above the corrected watermark, and how many were dropped so
        // resume can re-append them cleanly.
        let trimmed: Vec<String> = repair
            .trimmed
            .iter()
            .map(|(namespace, count)| format!("{namespace}={count}"))
            .collect();
        warn!(
            claimed = u32::from(repair.claimed),
            corrected = u32::from(repair.corrected),
            trimmed = trimmed.join(","),
            "store watermark was ahead of its headers index; corrected to the highest header held \
             and trimmed walk-ordered entries above it"
        );
    }
    Ok(())
}

/// A [`ReachabilityProbe`] that always reports reachable: the caller confirmed
/// the validator before building the client the assembly is booted over.
struct AlreadyReachable;

impl ReachabilityProbe for AlreadyReachable {
    async fn reachable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use zaino_primitives::types::{
        AbsoluteChainWork, BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds,
        Height, NetworkUpgradeInfo, NetworkUpgradeStatus, ValuePoolBalance, Zatoshis,
        NU5_BRANCH_ID, SAPLING_BRANCH_ID,
    };
    use zaino_source::{
        GetBlockchainInfoError, NonDomainError, OneShotGetBlockchainInfo, QueryError, RetryPolicy,
        ValidatorClient, ValidatorSource,
    };

    use super::{read_pool_activations, DeployError};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid height")
    }

    /// A blockchain-info aggregate whose only test-relevant field is the upgrade
    /// schedule; the rest are neutral, mirroring the service mock's synthesis.
    fn info_with(upgrades: Vec<NetworkUpgradeInfo>) -> BlockchainInfo {
        BlockchainInfo {
            chain: "main".to_string(),
            blocks: height(0),
            headers: height(0),
            estimated_height: height(0),
            best_block_hash: BlockHash::ZERO,
            difficulty: 0.0,
            verification_progress: 1.0,
            chain_work: Option::<AbsoluteChainWork>::None,
            pruned: false,
            size_on_disk: 0,
            commitments: 0,
            chain_supply: ValuePoolBalance {
                id: "transparent".to_string(),
                chain_value: Zatoshis::ZERO,
                monitored: true,
                value_delta: None,
            },
            value_pools: Vec::new(),
            upgrades,
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0),
                next_block: ConsensusBranchId::new(0),
            },
        }
    }

    fn upgrade(branch_id: u32, activation: u32) -> NetworkUpgradeInfo {
        NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch_id),
            name: "upgrade".to_string(),
            activation_height: height(activation),
            status: NetworkUpgradeStatus::Active,
        }
    }

    /// A validator source that counts schedule reads, answering a scripted
    /// result. It implements the one-shot port (the resilient `GetBlockchainInfo`
    /// the boot read bounds on is sealed to `ValidatorClient`, which wraps this).
    struct Spy {
        calls: Arc<AtomicUsize>,
        answer: Result<BlockchainInfo, GetBlockchainInfoError>,
    }

    impl ValidatorSource for Spy {
        type NonDomain = NonDomainError;
    }

    impl OneShotGetBlockchainInfo for Spy {
        async fn get_blockchain_info(
            &self,
        ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.clone().map_err(QueryError::Domain)
        }
    }

    fn client(spy: Spy) -> ValidatorClient<Spy> {
        ValidatorClient::new(spy, RetryPolicy::default())
    }

    /// Step 1(b): boot reads the validator's schedule exactly once and projects
    /// it per pool by consensus branch id. A pool the schedule omits (Ironwood
    /// here, as on a network without NU6.3) is unscheduled.
    #[tokio::test]
    async fn boot_reads_the_activation_schedule_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client(Spy {
            calls: Arc::clone(&calls),
            answer: Ok(info_with(vec![
                upgrade(SAPLING_BRANCH_ID, 419_200),
                upgrade(NU5_BRANCH_ID, 1_687_104),
            ])),
        });

        let activations = read_pool_activations(&client).await.expect("schedule read");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the schedule is read exactly once at boot"
        );
        assert_eq!(activations.sapling, Some(height(419_200)));
        assert_eq!(activations.orchard, Some(height(1_687_104)));
        assert_eq!(activations.ironwood, None);
    }

    /// A validator that cannot describe its chain fails the boot with a typed
    /// error rather than silently defaulting to a wrong schedule.
    #[tokio::test]
    async fn boot_fails_when_the_schedule_is_unavailable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client(Spy {
            calls,
            answer: Err(GetBlockchainInfoError::NotReady),
        });
        assert!(matches!(
            read_pool_activations(&client).await,
            Err(DeployError::ActivationSchedule(_))
        ));
    }
}
