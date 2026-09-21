//! A runnable, deterministic, offline walk of the FS⊕NFS compact-block seam,
//! driven through the *real* components and narrated through the *real*
//! observability stack.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p zaino-chainview --example seam_run
//! ```
//!
//! It composes a genuine finalised store (`zaino-store`'s `StoreReader`, indexed
//! by the runtime's sync stack over an offline `MockChain`) with a genuine
//! non-finalised head (`zaino-chain-head-service`'s `ChainHeadService`, driven
//! deterministically over a hand-rolled offline validator). The two are wired so
//! an **initial-build gap** exists: an on-chain height band that the finalised
//! store has not yet built up to and the volatile head's retained window does not
//! reach down to. The demo reads one height in each seam region and streams a
//! range across the whole span, so a human can *watch* the gap surface as a typed
//! `NotServiceable(Blocks)` through the same `tracing` sink every other event
//! flows through.
//!
//! ```text
//! FS  = [0, W]        finalised, durable          → Ok(Some(block))
//! gap = (W, F)        on-chain, held by neither    → Err(NotServiceable(Blocks))
//! NFS = [F, T]        volatile, retained window    → Ok(Some(block))
//! above (T, ∞)                                     → Ok(None)
//! ```
//!
//! Everything is offline and deterministic; the example self-verifies each
//! region with `matches!` so a regression fails it loudly.

use std::num::NonZeroU32;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadConfig, ChainHeadSnapshot as _};
use zaino_chain_head_service::ChainHeadService;
use zaino_chainview::ChainView;
use zaino_component::{ComponentName, ReachabilityProbe};
use zaino_core::{BlockRef, Capability, CompactBlock, Height, HeightRange};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CompactDifficulty,
    EquihashSolution, MerkleRoot, TreeRoots,
};
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::{ChainSegment, CompactBlockRead, Snapshot, TakeSnapshot};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{
    GetBlockByHashError, GetBlockError, GetChainTipError, GetCommitmentTreeRootsError,
    OneShotGetBlock, OneShotGetBlockByHash, OneShotGetChainTip, OneShotGetCommitmentTreeRoots,
    QueryError, RetryPolicy, SubscribeBlocks, ValidatorClient, ValidatorSource,
};
use zaino_store::StoreReader;

/// The log target for this example's own narration. It begins with `zaino` so
/// the default `zaino-logging` filter (`zaino=info,…`) enables it alongside every
/// event the real components emit — the whole run reads as one interleaved log.
const LOG: &str = "zaino::seam_run";

/// Finalised store tip: the finalised, durable prefix is `[0, W]`.
const W: u32 = 8;
/// Non-finalised chain tip: the validator's best chain is `[0, T]`.
const T: u32 = 20;
/// The head's retained depth below the tip. The window floor comes out at
/// `T - MAX_DEPTH` (the head anchors there and a single fast-forward never trims
/// above the anchor), which is verified at runtime below.
const MAX_DEPTH: u32 = 5;

/// A reachable validator, for booting the finalised store's runtime.
struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

fn height(value: u32) -> Height {
    Height::try_from(value).expect("valid demo height")
}

/// The demo hashing scheme, shared by both sides of the seam: block `h` hashes to
/// `[10 + h; 32]`, so a height means the same block whether it is served from the
/// finalised store or the non-finalised head.
fn hash_of(h: u32) -> BlockHash {
    let byte = u8::try_from(10 + h).expect("small demo height fits a hash byte");
    BlockHash::from([byte; 32])
}

/// A block for the offline head's validator: the shared hash scheme, but with the
/// parent linkage the head needs to extend one block at a time (`test_block`
/// zeroes `prev_hash`, which the finalised store's indexer does not care about
/// but the head's chain walk does).
fn linked_block(h: u32) -> Block {
    Block {
        header: BlockHeader {
            hash: hash_of(h),
            version: 4,
            prev_hash: if h == 0 {
                BlockHash::ZERO
            } else {
                hash_of(h - 1)
            },
            height: height(h),
            time: 0,
            merkle_root: MerkleRoot::from([0; 32]),
            block_commitments: BlockCommitments::from([0; 32]),
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: vec![],
        chain_metadata: ChainMetadata::ZERO,
    }
}

/// A hand-rolled offline validator: a fixed best chain `[0, T]`, answering only
/// the questions the chain head asks. Static, so no query ever fails.
#[derive(Clone)]
struct OfflineValidator {
    /// The best chain, indexed by height. `best_chain[h]` is the block at `h`.
    best_chain: Arc<Vec<Block>>,
}

impl OfflineValidator {
    /// A best chain `[0, tip]`, each block hashed and linked by the shared scheme.
    fn linear(tip: u32) -> Self {
        let best_chain = (0..=tip).map(linked_block).collect();
        Self {
            best_chain: Arc::new(best_chain),
        }
    }
}

impl ValidatorSource for OfflineValidator {
    type NonDomain = zaino_source::NonDomainError;
}

impl OneShotGetChainTip for OfflineValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let tip = self
            .best_chain
            .last()
            .expect("offline validator chain is never empty");
        Ok((tip.header.hash, tip.header.height))
    }
}

impl OneShotGetBlock for OfflineValidator {
    async fn get_block(&self, at: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.best_chain
            .get(usize::try_from(u32::from(at)).expect("height fits usize"))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(at)))
    }
}

impl OneShotGetBlockByHash for OfflineValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.best_chain
            .iter()
            .find(|block| block.header.hash == hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetCommitmentTreeRoots for OfflineValidator {
    async fn get_commitment_tree_roots(
        &self,
        _block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        // All-`None`: the head carries zero ChainMetadata for these blocks, which
        // is fine for a seam-routing demo — the point is *which side answers*, not
        // the note commitment sizes.
        Ok(TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        })
    }
}

// A latency hint only, and the demo steps the head by hand, so the default
// (no push channel) is exactly right.
impl SubscribeBlocks for OfflineValidator {}

/// Build a finalised store indexed over `[0, tip]` by running the real sync stack
/// over an offline `MockChain`, booted under the orchestra until every component
/// is `Ready`. Finalised depth is zero, so the watermark is exactly `tip`.
async fn build_finalised_store(tip: u32) -> StoreReader<InMemoryBackend> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for h in 0..=tip {
        let byte = u8::try_from(10 + h).expect("small demo height fits a hash byte");
        chain = chain.with_block(test_block(h, byte));
    }
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        index_set(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            finalised_depth: 0,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
    )
    .expect("sync driver builds");

    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let reader = StoreReader::new(Arc::new(backend.clone()));
    let store = zaino_store::StoreComponent::new(ComponentName("store"), reader.clone());
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots")
        .boot(store)
        .await
        .expect("store boots")
        .build();

    // Booting spawns the indexer; it catches up asynchronously. Keep the
    // orchestra alive and poll the store's committed watermark until it reaches
    // `tip`, so the reader we hand back covers `[0, tip]` in full. Everything read
    // is already in the backend `Arc`, so it survives the orchestra being dropped.
    let mut waits = 0;
    loop {
        let covered = reader
            .snapshot()
            .await
            .expect("store snapshot")
            .coverage()
            .map(|range| u32::from(range.end));
        if covered == Some(tip) {
            break;
        }
        waits += 1;
        assert!(waits <= 400, "indexer never reached watermark {tip}");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    for status in orchestra.statuses() {
        tracing::info!(
            target: LOG,
            component = %status.name,
            lifecycle = ?status.lifecycle,
            "finalised-store component booted",
        );
    }
    reader
}

/// Build a non-finalised head over the offline validator and step it — with no
/// writer task running, so the demo is the only thing advancing the graph — until
/// its published tip reaches `tip`.
async fn build_non_finalised_head(
    tip: u32,
    max_depth: u32,
) -> Arc<ChainHeadService<OfflineValidator>> {
    let validator = OfflineValidator::linear(tip);
    let config = ChainHeadConfig::with_max_depth(
        NonZeroU32::new(max_depth).expect("demo max_depth is not zero"),
    );
    // No finalised store drives the demo, so the head is told the store has
    // confirmed nothing: it retains everything down to its anchor and never
    // trims a height the (absent) finalised side cannot serve.
    let (_confirmed, confirmed_watermark) = watch::channel::<Option<Height>>(None);
    let head = ChainHeadService::spawn_without_writer(
        Arc::new(validator),
        config,
        confirmed_watermark,
        CancellationToken::new(),
    )
    .await
    .expect("offline validator is reachable, so the head anchors");

    // A single `advance_once` fast-forwards from the anchor to the fixed tip; the
    // loop is bounded so a stall surfaces as a panic rather than a hang.
    let mut steps = 0;
    while u32::from(head.subscriber().current().best_tip().height) < tip {
        head.advance_once().await.expect("advance succeeds");
        steps += 1;
        assert!(steps <= 100, "head never reached tip {tip}");
    }
    head
}

/// Read one height through the composed seam, log the outcome through the real
/// sink, and hand the result back for the caller to assert on.
async fn read_and_log(
    snapshot: &impl CompactBlockRead,
    region: &str,
    h: u32,
) -> Result<Option<CompactBlock>, BlockReadError> {
    let result = snapshot.compact_block(BlockRef::Height(height(h))).await;
    match &result {
        Ok(Some(block)) => tracing::info!(
            target: LOG,
            region,
            height = block.height,
            "served block at {h} ({region})",
        ),
        Ok(None) => tracing::info!(
            target: LOG,
            region,
            height = h,
            "above tip: no block at {h} ({region})",
        ),
        Err(BlockReadError::NotServiceable(capability)) => tracing::warn!(
            target: LOG,
            region,
            height = h,
            "gap: {h} not serviceable ({capability:?})",
        ),
        Err(other) => tracing::warn!(
            target: LOG,
            region,
            height = h,
            "unexpected read error at {h}: {other:?}",
        ),
    }
    result
}

#[tokio::main]
async fn main() {
    // Every step from here logs through the real stack. Nothing else installs a
    // subscriber, so this is the sink the components' own events flow through too.
    zaino_logging::try_init();

    tracing::info!(
        target: LOG,
        w = W,
        t = T,
        max_depth = MAX_DEPTH,
        "composing the FS⊕NFS seam: finalised store [0, W], non-finalised head [F, T]",
    );

    // --- The finalised store (FS), indexed over [0, W]. ---
    let store = build_finalised_store(W).await;
    let fs_coverage = store
        .snapshot()
        .await
        .expect("store snapshot")
        .coverage()
        .expect("the finalised store holds blocks");
    tracing::info!(
        target: LOG,
        start = u32::from(fs_coverage.start),
        end = u32::from(fs_coverage.end),
        "FS coverage (watermark = end)",
    );
    assert_eq!(u32::from(fs_coverage.start), 0, "FS floor is genesis");
    assert_eq!(u32::from(fs_coverage.end), W, "FS watermark is W");

    // --- The non-finalised head (NFS), a retained window [F, T]. ---
    let head = build_non_finalised_head(T, MAX_DEPTH).await;
    let subscriber = head.subscriber();
    let nfs_coverage = subscriber
        .snapshot()
        .await
        .expect("head snapshot")
        .coverage()
        .expect("the head always holds a window");
    let floor = u32::from(nfs_coverage.start);
    let head_tip = u32::from(nfs_coverage.end);
    tracing::info!(
        target: LOG,
        floor,
        tip = head_tip,
        "NFS retained window (floor = F, tip = T)",
    );
    assert_eq!(head_tip, T, "NFS tip is T");
    assert!(
        floor > W + 1,
        "the demo needs an initial-build gap: NFS floor {floor} must sit above W+1 = {}",
        W + 1,
    );

    // Representative heights, one per region, derived from the achieved bounds.
    let finalised_h = W / 2; // inside [0, W]
    let gap_h = (W + floor) / 2; // strictly between W and the floor
    let volatile_h = floor + (T - floor) / 2; // inside [F, T]
    let above_h = T + 5; // beyond the tip
    tracing::info!(
        target: LOG,
        finalised_h,
        gap_h,
        volatile_h,
        above_h,
        "regions: finalised (0,W] | gap (W,F) | volatile [F,T] | above (T,∞)",
    );

    // --- Compose and pin one coherent snapshot across the seam. ---
    let view = ChainView::new(store, subscriber);
    let snap = view.snapshot().await.expect("compose a pinned snapshot");

    let coverage = snap.coverage().expect("the composed view covers a span");
    let serviceable = snap.serviceable_range();
    tracing::info!(
        target: LOG,
        coverage_start = u32::from(coverage.start),
        coverage_end = u32::from(coverage.end),
        finalized_tip = u32::from(serviceable.finalized_tip),
        tip = u32::from(serviceable.tip),
        "composed snapshot: coverage = FS ∪ NFS, serviceable_range = (watermark, served tip)",
    );
    assert_eq!(u32::from(serviceable.finalized_tip), W, "watermark is W");
    assert_eq!(u32::from(serviceable.tip), T, "served tip is the NFS tip");

    // --- The four seam routes, each read through the real observability. ---
    let finalised = read_and_log(&snap, "finalised", finalised_h).await;
    assert!(
        matches!(finalised, Ok(Some(_))),
        "finalised height {finalised_h} must serve a block, got {finalised:?}",
    );

    let gap = read_and_log(&snap, "gap", gap_h).await;
    assert!(
        matches!(gap, Err(BlockReadError::NotServiceable(Capability::Blocks))),
        "gap height {gap_h} must be NotServiceable(Blocks), got {gap:?}",
    );

    let volatile = read_and_log(&snap, "volatile", volatile_h).await;
    assert!(
        matches!(volatile, Ok(Some(_))),
        "volatile height {volatile_h} must serve a block, got {volatile:?}",
    );

    let above = read_and_log(&snap, "above", above_h).await;
    assert!(
        matches!(above, Ok(None)),
        "above-tip height {above_h} must be Ok(None), got {above:?}",
    );

    // --- The range stitch: one eager per-height stream across the whole span. ---
    let span_start = 3;
    tracing::info!(
        target: LOG,
        start = span_start,
        end = T,
        "streaming compact blocks across [3, T] — an eager per-height stitch over the seam",
    );
    let items: Vec<Result<CompactBlock, ReadError>> = snap
        .stream_compact(HeightRange {
            start: height(span_start),
            end: height(T),
        })
        .collect()
        .await;

    let served = items.iter().filter(|item| item.is_ok()).count();
    let gaps = items
        .iter()
        .filter(|item| matches!(item, Err(ReadError::NotServiceable(_))))
        .count();
    // With the whole span at or below the tip, no height is skipped as an
    // above-tip absence, so the streamed items map one-to-one onto `[3, T]` and
    // the first error's position names the first gap height.
    let first_gap_at = items
        .iter()
        .position(std::result::Result::is_err)
        .map(|index| span_start + u32::try_from(index).expect("stream index fits u32"));
    tracing::info!(
        target: LOG,
        served,
        gaps,
        first_gap_at,
        "range stitch complete: served blocks, gap items, first gap height",
    );
    if let Some(gap_height) = first_gap_at {
        tracing::warn!(
            target: LOG,
            height = gap_height,
            "the initial-build gap surfaces mid-stream as a NotServiceable item at {gap_height}",
        );
    }

    let expected_served =
        usize::try_from((W - span_start + 1) + (T - floor + 1)).expect("served count fits usize");
    let expected_gaps = usize::try_from(floor - W - 1).expect("gap count fits usize");
    assert_eq!(served, expected_served, "served blocks across the seam");
    assert_eq!(gaps, expected_gaps, "gap items across the seam");
    assert_eq!(
        first_gap_at,
        Some(W + 1),
        "the first gap is the height just above the watermark",
    );

    tracing::info!(
        target: LOG,
        "seam walk complete: all four regions verified and the gap surfaced as NotServiceable",
    );
}
