//! The FS⊕NFS route, exercised over a *real* finalised store (`zaino-store`'s
//! `StoreReader`, indexed by the runtime's sync stack) paired with the in-memory
//! [`StubNonFinalised`].
//!
//! The finalised store is indexed over `[0, watermark]`; the stub stands in for
//! the volatile window `[floor, tip]`. Each test pins one seam configuration and
//! asserts where a read is routed:
//!
//! ```text
//! FS = [genesis, watermark]   NFS = [floor, tip]   served = FS ∪ NFS
//! gap = (watermark, floor) → NotServiceable        above (tip, ∞) → Ok(None)
//! ```

use std::sync::Arc;

use futures::StreamExt;

use zaino_chainview::ChainView;
use zaino_chainview::testing::{StubNonFinalised, stub_compact_block};
use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::{BlockHash, BlockRef, Capability, Height, HeightRange};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::{CompactBlockRead, Snapshot, TakeSnapshot};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::{StoreComponent, StoreReader};

/// A reachable validator.
struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

fn height(value: u32) -> Height {
    Height::try_from(value).expect("valid test height")
}

fn range(start: u32, end: u32) -> HeightRange {
    HeightRange {
        start: height(start),
        end: height(end),
    }
}

/// A finalised store holding nothing — an empty backend, no indexing. Its
/// watermark is `None`.
fn empty_store() -> StoreReader<InMemoryBackend> {
    StoreReader::new(Arc::new(InMemoryBackend::new()))
}

/// A finalised store indexed over `[0, tip]`. Finalised depth is zero, so the
/// watermark is exactly `tip`. Block `h` carries hash `[10 + h; 32]`.
async fn indexed_store(tip: u32) -> StoreReader<InMemoryBackend> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for h in 0..=tip {
        let hash_byte = u8::try_from(10 + h).expect("small test height");
        chain = chain.with_block(test_block(h, hash_byte));
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
    .expect("driver builds");
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let reader = StoreReader::new(Arc::new(backend.clone()));
    let store = StoreComponent::new(ComponentName("store"), reader.clone());
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots")
        .boot(store.clone())
        .await
        .expect("store boots")
        .build();
    for status in orchestra.statuses() {
        assert_eq!(status.lifecycle, Lifecycle::Ready, "{}", status.name);
    }
    reader
}

/// A stub window over `[floor, tip]`, block `h` carrying hash `[10 + h; 32]` so
/// heights line up with the finalised store's hashing scheme across the seam.
fn stub_window(floor: u32, tip: u32) -> StubNonFinalised {
    let blocks = (floor..=tip)
        .map(|h| stub_compact_block(h, u8::try_from(10 + h).expect("small test height")))
        .collect();
    StubNonFinalised::from_blocks(blocks)
}

/// A range read returns the lower heights from the FS and the upper from the
/// NFS, in height order, and the coherence marker reports the composed tip.
#[tokio::test]
async fn range_read_stitches_across_the_seam() {
    // FS = [0, 2], NFS = [3, 5]; watermark = 2.
    let view = ChainView::new(indexed_store(2).await, stub_window(3, 5));
    let snapshot = view.snapshot().await.expect("snapshot");

    // Coherence marker: composed tip is the NFS tip; the serviceable range's
    // finalised tip is the watermark and its tip is the NFS tip.
    let tip = snapshot.pinned_tip().expect("composed tip");
    assert_eq!(u32::from(tip.height), 5, "composed tip is the NFS tip");
    let serviceable = snapshot.serviceable_range();
    assert_eq!(u32::from(serviceable.finalized_tip), 2, "watermark");
    assert_eq!(u32::from(serviceable.tip), 5, "served tip");

    // Heights 1,2 come from the FS; 3,4 from the NFS — in order.
    let blocks: Vec<u32> = snapshot
        .stream_compact(range(1, 4))
        .map(|item| item.expect("no gap in this range").height)
        .collect()
        .await;
    assert_eq!(blocks, vec![1, 2, 3, 4], "stitched in height order");

    // Point reads: an FS height and an NFS height, each hashed by its side.
    let fs_block = snapshot
        .compact_block(BlockRef::Height(height(2)))
        .await
        .expect("read")
        .expect("FS block at the watermark");
    assert_eq!(fs_block.hash, BlockHash::from([12; 32]), "from the FS");
    let nfs_block = snapshot
        .compact_block(BlockRef::Height(height(4)))
        .await
        .expect("read")
        .expect("NFS block in the window");
    assert_eq!(nfs_block.hash, BlockHash::from([14; 32]), "from the NFS");
}

/// An empty/lagging FS is normal: with no watermark, every height routes to the
/// NFS, and a young chain is served entirely from the non-finalised side.
#[tokio::test]
async fn empty_fs_routes_everything_to_the_nfs() {
    // FS empty (watermark None), NFS = [0, 3].
    let view = ChainView::new(empty_store(), stub_window(0, 3));
    let snapshot = view.snapshot().await.expect("snapshot");

    // The serviceable range's finalised tip collapses to genesis (no watermark),
    // while the served tip is the NFS tip.
    let serviceable = snapshot.serviceable_range();
    assert_eq!(
        u32::from(serviceable.finalized_tip),
        0,
        "no watermark → genesis"
    );
    assert_eq!(u32::from(serviceable.tip), 3, "served tip is the NFS tip");

    let blocks: Vec<u32> = snapshot
        .stream_compact(range(0, 3))
        .map(|item| item.expect("served from the NFS").height)
        .collect()
        .await;
    assert_eq!(blocks, vec![0, 1, 2, 3], "whole range from the NFS");

    let block = snapshot
        .compact_block(BlockRef::Height(height(2)))
        .await
        .expect("read")
        .expect("served from the NFS");
    assert_eq!(block.hash, BlockHash::from([12; 32]), "from the NFS");
}

/// With the NFS empty, heights up to the watermark are served from the FS and
/// anything above the watermark is `Ok(None)` — no such block.
#[tokio::test]
async fn fs_only_serves_up_to_the_watermark() {
    // FS = [0, 2], NFS empty; watermark = 2.
    let view = ChainView::new(indexed_store(2).await, StubNonFinalised::empty());
    let snapshot = view.snapshot().await.expect("snapshot");

    let below = snapshot
        .compact_block(BlockRef::Height(height(1)))
        .await
        .expect("read")
        .expect("FS block below the watermark");
    assert_eq!(below.height, 1);

    let above = snapshot
        .compact_block(BlockRef::Height(height(3)))
        .await
        .expect("read");
    assert!(
        above.is_none(),
        "above the watermark with an empty NFS → None"
    );
}

/// A height above the NFS tip is `Ok(None)`.
#[tokio::test]
async fn above_the_nfs_tip_is_none() {
    // FS = [0, 2], NFS = [3, 5]; watermark = 2, tip = 5.
    let view = ChainView::new(indexed_store(2).await, stub_window(3, 5));
    let snapshot = view.snapshot().await.expect("snapshot");

    let above = snapshot
        .compact_block(BlockRef::Height(height(6)))
        .await
        .expect("read");
    assert!(above.is_none(), "above the NFS tip → None");
}

/// The initial-build gap — a height above the watermark but below the NFS floor,
/// held by neither side while the FS is still building — is `NotServiceable`.
#[tokio::test]
async fn initial_build_gap_is_not_serviceable() {
    // FS = [0, 2], NFS = [5, 6]; watermark = 2, floor = 5 → gap = {3, 4}.
    let view = ChainView::new(indexed_store(2).await, stub_window(5, 6));
    let snapshot = view.snapshot().await.expect("snapshot");

    let gap = snapshot.compact_block(BlockRef::Height(height(3))).await;
    assert!(
        matches!(gap, Err(BlockReadError::NotServiceable(Capability::Blocks))),
        "a gap height is not serviceable, got {gap:?}"
    );

    // Over a stream, a gap height surfaces as a `NotServiceable` error item.
    let items: Vec<Result<u32, ReadError>> = snapshot
        .stream_compact(range(3, 4))
        .map(|item| item.map(|block| block.height))
        .collect()
        .await;
    assert!(
        items
            .iter()
            .all(|item| matches!(item, Err(ReadError::NotServiceable(Capability::Blocks)))),
        "gap heights stream as NotServiceable errors, got {items:?}"
    );
}

/// A hash read resolves `FS ∪ NFS`: the finalised store first, then the volatile
/// window; an unknown hash is `Ok(None)`.
#[tokio::test]
async fn hash_reads_resolve_across_both_sides() {
    // FS = [0, 2] (h2 hash [12]), NFS = [3, 5] (h3 hash [13]).
    let view = ChainView::new(indexed_store(2).await, stub_window(3, 5));
    let snapshot = view.snapshot().await.expect("snapshot");

    let fs_hit = snapshot
        .compact_block(BlockRef::Hash(BlockHash::from([12; 32])))
        .await
        .expect("read")
        .expect("FS block by hash");
    assert_eq!(fs_hit.height, 2, "resolved from the FS");

    let nfs_hit = snapshot
        .compact_block(BlockRef::Hash(BlockHash::from([13; 32])))
        .await
        .expect("read")
        .expect("NFS block by hash");
    assert_eq!(nfs_hit.height, 3, "resolved from the NFS");

    let miss = snapshot
        .compact_block(BlockRef::Hash(BlockHash::from([99; 32])))
        .await
        .expect("read");
    assert!(miss.is_none(), "unknown hash → None");
}
