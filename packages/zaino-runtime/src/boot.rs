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
use zaino_service::use_cases::{Serves, UseCase};
use zaino_service::TakeSnapshot;
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
    C: IndexedSource,
    StoreReader<LmdbBackend, D::Indexes>: TakeSnapshot<Snapshot: ChainTier>,
    IndexedEngine<D, C>: Serves<D::UseCase>,
    RunComponent<A>: StatusSource + StatusWatch + Managed + Clone + Send + Sync + 'static,
{
    let source = client;

    let backend = open_store::<D>(&config.store)?;

    // The finalised store: the indexer writes it, the engine composes blocks on
    // read from it. One reader, shared (Arc-backed clone). Typed to the
    // deployment's index set: the reads it has are exactly the reads those
    // indexes back.
    let store_reader = StoreReader::<_, D::Indexes>::new(Arc::new(backend.clone()));
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
            .map_err(DeployError::Indexer)?;
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
            .map_err(DeployError::Indexer)?;
            assemble::<D, A, C, _, _>(source, store_reader, driver, serve).await
        }
    }
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
        warn!(
            claimed = u32::from(repair.claimed),
            corrected = u32::from(repair.corrected),
            "store watermark was ahead of its headers index; corrected to the highest header held"
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
