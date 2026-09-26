//! How a chain view answers, given what each provider covers.
//!
//! One chain of 1201 blocks throughout; only where the store and the window sit
//! varies. That produces every coverage arrangement there is.

use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _};
use zaino_chain::testing::{
    chainwork_at, hash_of, height, Chain, FakeHead, FakeSource, FakeStore, SourceCall,
};
use zaino_chain::{
    Answerable, BlockId, BlockRead as _, ChainCapability, ChainScope, ChainView as _,
    ChainViewComposer, ChainViewConfig, ChainViewError, ChainViewSnapshot as _,
    CompactBlockRead as _, ForkReconcile as _, Locator, SpendRead as _, SpendStatus,
    TreestateRead as _,
};
use zaino_chain_store::PoolFilter;
use zaino_primitives::types::{BlockRef, SingleBlockWork};

fn chain() -> Chain {
    Chain::of_length(1201)
}

fn view(
    chain: &Chain,
    store_top: u32,
    floor: u32,
    tip: u32,
) -> ChainViewComposer<FakeStore, FakeHead, FakeSource> {
    ChainViewComposer::new(
        FakeStore::covering(chain, store_top),
        FakeHead::covering(chain, floor, tip),
        Arc::new(FakeSource::over(chain)),
        ChainViewConfig::default(),
    )
}

/// Steady state: store to 1000, window 1000..=1200 — the providers overlap.
fn caught_up(chain: &Chain) -> ChainViewComposer<FakeStore, FakeHead, FakeSource> {
    view(chain, 1000, 1000, 1200)
}

/// Catch-up: store to 100, window 1100..=1200 — a hole at 101..=1099.
fn syncing(chain: &Chain) -> ChainViewComposer<FakeStore, FakeHead, FakeSource> {
    view(chain, 100, 1100, 1200)
}

// ***** Provider selection *****

#[tokio::test]
async fn the_providers_split_at_the_watermark() {
    let chain = chain();
    let snapshot = caught_up(&chain).snapshot();
    assert_eq!(
        snapshot.block_hash(height(999)).await.expect("serviceable"),
        Some(hash_of(999))
    );
    assert_eq!(
        snapshot
            .block_hash(height(1100))
            .await
            .expect("serviceable"),
        Some(hash_of(1100))
    );
}

/// Where both cover a height, the store answers and the validator is untouched.
///
/// Chainwork is the fingerprint: only the store carries it.
#[tokio::test]
async fn the_store_wins_the_overlap() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        FakeHead::covering(&chain, 1000, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );

    let block = composer
        .snapshot()
        .block(BlockId::Height(height(1000)))
        .await
        .expect("serviceable")
        .expect("covered");

    assert!(block.chainwork.is_some(), "the store should have answered");
    assert!(source.calls().is_empty(), "the validator was consulted");
}

#[tokio::test]
async fn a_height_above_the_tip_is_a_miss() {
    let chain = chain();
    assert_eq!(
        caught_up(&chain)
            .snapshot()
            .block_hash(height(5000))
            .await
            .expect("serviceable"),
        None
    );
}

// ***** The hole *****

/// A height in the hole is filled from the validator, not refused.
#[tokio::test]
async fn a_height_in_the_hole_is_filled() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 100),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );

    assert_eq!(
        composer
            .snapshot()
            .block_hash(height(500))
            .await
            .expect("fillable"),
        Some(hash_of(500))
    );
    assert!(source
        .calls()
        .iter()
        .any(|call| matches!(call, SourceCall::Block(_))));
}

/// A streamed range spanning store, hole and window is contiguous and ordered.
///
/// The shape a per-capability route/merge/passthrough model cannot express, and
/// the one a wallet sync actually asks for.
#[tokio::test]
async fn a_stream_spanning_all_three_providers_is_contiguous() {
    let chain = chain();
    let snapshot = syncing(&chain).snapshot();

    let chunks: Vec<_> = snapshot
        .stream_compact(height(98), height(1102), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable");

    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (98..=1102).collect::<Vec<_>>());
}

/// The same, for indexed blocks, and chainwork marks who answered.
#[tokio::test]
async fn a_block_stream_shows_where_chainwork_stops() {
    let chain = chain();
    let blocks: Vec<_> = syncing(&chain)
        .snapshot()
        .stream_blocks(height(98), height(1102))
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 1005);
    assert!(blocks[0].chainwork.is_some(), "98 is in the store");
    assert!(blocks[2].chainwork.is_some(), "100 is the watermark");
    assert!(blocks[3].chainwork.is_none(), "101 is in the hole");
    assert!(
        blocks.last().expect("non-empty").chainwork.is_none(),
        "1102 is in the window, above the hole"
    );
}

#[tokio::test]
async fn the_hole_is_reported() {
    let chain = chain();
    let range = syncing(&chain).snapshot().serviceable_range();
    assert_eq!(range.finalised_tip, Some(height(100)));
    assert_eq!(range.tip, Some(height(1200)));
    assert_eq!(range.gap_from, Some(height(101)));

    assert_eq!(
        caught_up(&chain).snapshot().serviceable_range().gap_from,
        None
    );
}

/// With the validator off, the hole becomes unserviceable — including mid-stream.
#[tokio::test]
async fn a_hole_is_unserviceable_without_the_validator() {
    let chain = chain();
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 100),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default().without_passthrough(),
    );
    let snapshot = composer.snapshot();

    assert!(matches!(
        snapshot.block_hash(height(500)).await,
        Err(ChainViewError::NotServiceable(_))
    ));
    assert!(snapshot.block_hash(height(50)).await.is_ok());
    assert!(snapshot.block_hash(height(1150)).await.is_ok());

    let streamed: Result<Vec<_>, _> = snapshot
        .stream_compact(height(50), height(1150), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await;
    assert!(matches!(streamed, Err(ChainViewError::NotServiceable(_))));
}

#[tokio::test]
async fn a_disabled_store_leaves_the_validator_covering_the_chain() {
    let chain = chain();
    let composer = ChainViewComposer::new(
        FakeStore::empty(),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default().without_store(),
    );
    let snapshot = composer.snapshot();

    assert_eq!(
        snapshot.block_hash(height(500)).await.expect("filled"),
        Some(hash_of(500))
    );
    assert_eq!(snapshot.serviceable_range().finalised_tip, None);
}

// ***** Streaming behaviour *****

/// A stream yields several chunks, and they tile the range exactly.
///
/// Chunking is what keeps memory bounded for a client syncing the whole chain;
/// a single chunk would mean the `Vec` this crate exists to avoid.
#[tokio::test]
async fn a_stream_yields_multiple_chunks_that_tile_the_range() {
    let chain = chain();
    let chunks: Vec<Vec<_>> = caught_up(&chain)
        .snapshot()
        .stream_compact(height(0), height(1200), PoolFilter::all())
        .try_collect()
        .await
        .expect("serviceable");

    assert!(
        chunks.len() > 1,
        "expected chunking, got {} chunk(s)",
        chunks.len()
    );
    assert!(chunks.iter().all(|chunk| !chunk.is_empty()));

    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (0..=1200).collect::<Vec<_>>());
}

/// A stream is lazy: nothing is fetched until it is polled.
///
/// The backpressure property. A client that stops reading stops causing work,
/// which is what keeps thousands of them from each buffering a range.
#[tokio::test]
async fn a_stream_does_no_work_until_polled() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 100),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    let stream = snapshot.stream_compact(height(200), height(900), PoolFilter::all());
    assert!(
        source.calls().is_empty(),
        "constructing a stream must not fetch: {:?}",
        source.calls()
    );

    let mut stream = Box::pin(stream);
    let first = stream.next().await.expect("a chunk").expect("serviceable");
    assert!(!first.is_empty());
    let after_one = source.calls().len();

    // Dropping without draining stops the work.
    drop(stream);
    assert_eq!(
        source.calls().len(),
        after_one,
        "a dropped stream kept working"
    );
    assert!(
        after_one < 700,
        "one chunk fetched the whole range: {after_one} calls"
    );
}

/// A compact fill uses the compact port, not whole blocks, and pairs it with
/// the by-height verbose port so both requests are independent.
#[tokio::test]
async fn a_compact_fill_uses_the_compact_and_verbose_ports() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 100),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );

    let _ = composer
        .snapshot()
        .compact_block(height(500), PoolFilter::all())
        .await
        .expect("fillable");

    let calls = source.calls();
    assert!(calls.contains(&SourceCall::CompactBlock(height(500))));
    assert!(calls.contains(&SourceCall::BlockVerbose(height(500))));
    assert!(
        !calls.iter().any(|c| matches!(c, SourceCall::Block(_))),
        "a compact read must not fetch whole blocks: {calls:?}"
    );
}

// ***** Addressing *****

#[tokio::test]
async fn a_block_is_reachable_by_height_and_by_hash() {
    let chain = chain();
    let snapshot = caught_up(&chain).snapshot();
    for at in [50u32, 1150] {
        let by_height = snapshot
            .block(BlockId::Height(height(at)))
            .await
            .expect("serviceable")
            .expect("covered");
        let by_hash = snapshot
            .block(BlockId::Hash(hash_of(at)))
            .await
            .expect("serviceable")
            .expect("covered");
        assert_eq!(by_height.header.hash, by_hash.header.hash);
    }
}

/// A raw block by height is one round trip, and works inside the hole.
#[tokio::test]
async fn a_raw_block_by_height_is_fetched_by_height() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 100),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );

    assert!(composer
        .snapshot()
        .raw_block(BlockId::Height(height(500)))
        .await
        .expect("serviceable")
        .is_some());
    assert_eq!(source.calls(), vec![SourceCall::RawBlock(height(500))]);
}

/// A treestate by height goes straight to the validator, no resolution hop.
#[tokio::test]
async fn a_treestate_by_height_is_fetched_by_height() {
    let chain = chain();
    let source = FakeSource::over(&chain);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        FakeHead::covering(&chain, 1000, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default(),
    );

    assert!(composer
        .snapshot()
        .treestate(BlockId::Height(height(500)))
        .await
        .expect("serviceable")
        .is_some());
    assert_eq!(source.calls(), vec![SourceCall::Treestate(height(500))]);
}

// ***** Fork reconciliation *****

/// A locator resolves to the newest hash still on the chain.
///
/// What a client resyncing after a reorg asks. A single hash cannot express it:
/// the client's own tip may be orphaned, which is why it offers a run.
#[tokio::test]
async fn a_locator_finds_the_newest_surviving_hash() {
    use zaino_chain::testing::FakeHeadSnapshot;

    let chain = chain();
    let orphan = FakeHeadSnapshot::branch_block(1150, 9_999);
    let orphan_hash = orphan.reference.hash;

    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        FakeHead::new(FakeHeadSnapshot::covering(1100, 1200).with_branch_block(orphan)),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    // The client's tip is orphaned; the next hash back is canonical.
    let locator = Locator::new(vec![orphan_hash, hash_of(1149), hash_of(1148)]);
    assert_eq!(
        snapshot.fork_point(&locator).await.expect("serviceable"),
        Some(BlockRef {
            hash: hash_of(1149),
            height: height(1149)
        })
    );

    // A locator of only unknown hashes finds nothing.
    let unknown = Locator::new(vec![hash_of(90_001), hash_of(90_002)]);
    assert_eq!(
        snapshot.fork_point(&unknown).await.expect("serviceable"),
        None
    );
}

/// A branch hash has no best-chain height.
#[tokio::test]
async fn a_competing_branch_hash_has_no_best_chain_height() {
    use zaino_chain::testing::FakeHeadSnapshot;

    let chain = chain();
    let orphan = FakeHeadSnapshot::branch_block(1150, 9_999);
    let orphan_hash = orphan.reference.hash;

    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        FakeHead::new(FakeHeadSnapshot::covering(1100, 1200).with_branch_block(orphan)),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    assert_eq!(
        snapshot
            .block_height(orphan_hash)
            .await
            .expect("serviceable"),
        None
    );
    assert_eq!(
        snapshot
            .block_hash(height(1150))
            .await
            .expect("serviceable"),
        Some(hash_of(1150))
    );
}

// ***** Reads needing Zaino's own capabilities *****

/// A full-chain spend search refuses to span a hole; a finalised one does not.
#[tokio::test]
async fn a_full_chain_spend_search_refuses_to_span_a_hole() {
    let chain = chain();
    let snapshot = syncing(&chain).snapshot();
    let outpoint = zaino_primitives::types::Outpoint {
        txid: zaino_chain::testing::txid(1),
        index: 0,
    };

    assert!(matches!(
        snapshot
            .outpoint_spenders(&[outpoint], ChainScope::FullChain)
            .await,
        Err(ChainViewError::NotServiceable(_))
    ));
    assert_eq!(
        snapshot
            .outpoint_spenders(&[outpoint], ChainScope::Finalised)
            .await
            .expect("the store covers its own range"),
        vec![SpendStatus::Unspent]
    );
}

#[tokio::test]
async fn a_full_chain_spend_search_works_without_a_hole() {
    let chain = chain();
    let outpoint = zaino_primitives::types::Outpoint {
        txid: zaino_chain::testing::txid(1),
        index: 0,
    };
    assert_eq!(
        caught_up(&chain)
            .snapshot()
            .outpoint_spenders(&[outpoint], ChainScope::FullChain)
            .await
            .expect("contiguous"),
        vec![SpendStatus::Unspent]
    );
}

// ***** Serviceability *****

/// A hole caps what Zaino's own capabilities can answer, but not block reads.
///
/// The two halves of the same coverage: the validator can stand in for a block,
/// and cannot stand in for a spend index.
#[tokio::test]
async fn a_hole_caps_local_only_capabilities_and_not_block_reads() {
    let chain = chain();
    let manifest = syncing(&chain).serviceability();

    assert_eq!(
        manifest.get(ChainCapability::Blocks),
        Answerable::ToHeight(height(1200))
    );
    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::ToHeight(height(100)),
    );
}

/// A store without an index makes the capability it backs absent.
#[tokio::test]
async fn a_missing_index_makes_a_local_only_capability_absent() {
    use zaino_chain_store::StoreCapability;

    let chain = chain();
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000)
            .with_capabilities([StoreCapability::Core, StoreCapability::StoredBlocks]),
        FakeHead::covering(&chain, 1000, 1200),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );

    let manifest = composer.serviceability();
    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::Absent
    );
    // A shared payload is still served — from the validator, more slowly.
    assert_eq!(
        manifest.get(ChainCapability::CompactBlocks),
        Answerable::ToHeight(height(1200))
    );
}

/// Every advertised ceiling is actually readable.
///
/// The property the manifest exists for: it must not promise what the reads
/// refuse.
#[tokio::test]
async fn every_advertised_ceiling_is_readable() {
    let chain = chain();
    let composer = syncing(&chain);
    let manifest = composer.serviceability();
    let snapshot = composer.snapshot();

    for (capability, answerable) in manifest.iter() {
        let Answerable::ToHeight(top) = answerable else {
            continue;
        };
        match capability {
            ChainCapability::Blocks => {
                assert!(
                    snapshot
                        .block_hash(top)
                        .await
                        .expect("advertised")
                        .is_some(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            ChainCapability::CompactBlocks => {
                assert!(
                    snapshot
                        .compact_block(top, PoolFilter::all())
                        .await
                        .expect("advertised")
                        .is_some(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            ChainCapability::SpendStatus => {
                let outpoint = zaino_primitives::types::Outpoint {
                    txid: zaino_chain::testing::txid(1),
                    index: 0,
                };
                assert!(
                    snapshot
                        .outpoint_spenders(&[outpoint], ChainScope::Finalised)
                        .await
                        .is_ok(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            _ => {}
        }
    }
}

// ***** Coherence *****

#[tokio::test]
async fn a_snapshot_survives_a_reorg_underneath_it() {
    use zaino_chain::testing::FakeHeadSnapshot;

    let chain = chain();
    let head = FakeHead::covering(&chain, 1100, 1200);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        head.clone(),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );

    let before = composer.snapshot();
    assert_eq!(before.tip().height, height(1200));

    head.publish(FakeHeadSnapshot::covering(1100, 1150).at_generation(1));

    assert_eq!(before.tip().height, height(1200), "the pinned view moved");
    assert_eq!(composer.snapshot().tip().height, height(1150));
}

#[tokio::test]
async fn a_clone_answers_from_the_same_pinned_view() {
    use zaino_chain::testing::FakeHeadSnapshot;

    let chain = chain();
    let head = FakeHead::covering(&chain, 1100, 1200);
    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1000),
        head.clone(),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );

    let original = composer.snapshot();
    let cloned = original.clone();
    head.publish(FakeHeadSnapshot::covering(1100, 1150).at_generation(1));

    assert_eq!(original.tip(), cloned.tip());
    assert_eq!(cloned.tip().height, height(1200));
}

// ***** Pluggability *****

/// A store that builds only what a wallet needs still yields a working view.
///
/// The property an earlier version of this crate broke: it demanded every store
/// index at once, so a wallet-only deployment could not construct a chain view
/// at all — not "could not answer spend status", could not build.
///
/// `MinimalStore` implements neither the spend index nor the txout accumulator,
/// so the composed snapshot does not implement `SpendRead` or `TxOutSetRead`.
/// Those capabilities are absent at compile time, which is what the split read
/// traits exist to achieve — and the reads it *does* offer work unchanged.
#[tokio::test]
async fn a_minimal_store_yields_a_working_view() {
    use zaino_chain::testing::MinimalStore;

    let chain = chain();
    let composer = ChainViewComposer::new(
        MinimalStore::covering(&chain, 1000),
        FakeHead::covering(&chain, 1000, 1200),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    assert_eq!(
        snapshot.block_hash(height(500)).await.expect("serviceable"),
        Some(hash_of(500))
    );
    let blocks: Vec<_> = snapshot
        .stream_compact(height(0), height(1200), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(blocks.len(), 1201);

    // The capabilities it cannot back are reported absent, not merely unused.
    let manifest = composer.serviceability();
    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::Absent
    );
    assert_eq!(manifest.get(ChainCapability::TxOutSet), Answerable::Absent);
}

// ***** Chainwork above the finalised seam *****

/// A block the chain head answered for carries absolute chainwork once the
/// store has built as far as the anchor, and it is the value the store itself
/// would hold.
///
/// This is the whole point of the rebase. The chain head measures work from its
/// own anchor — the parent of the window floor — so the absolute value is
/// `chainwork(anchor) + work(B)`. Asserting the number rather than
/// `is_some` is what makes this a test of the arithmetic: an off-by-one in the
/// anchor, counting the floor's work twice, or rebasing against the wrong
/// height all produce a plausible `Some` and a wrong chain.
#[tokio::test]
async fn a_head_block_is_rebased_onto_the_anchors_chainwork() {
    let chain = chain();

    let block = caught_up(&chain)
        .snapshot()
        .block(BlockId::Height(height(1100)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(
        block.chainwork,
        Some(chainwork_at(1100)),
        "the head's answer must agree with what the store will hold for 1100",
    );
}

/// The seam is invisible in the chainwork: the store's last block and the
/// head's first differ by exactly one block's work.
#[tokio::test]
async fn chainwork_is_continuous_across_the_finalised_seam() {
    let chain = chain();
    let snapshot = caught_up(&chain).snapshot();

    // 1000 is the watermark and the store wins the overlap; 1001 is the first
    // height only the window covers.
    let finalised = snapshot
        .block(BlockId::Height(height(1000)))
        .await
        .expect("serviceable")
        .expect("covered")
        .chainwork
        .expect("the store answered");
    let recent = snapshot
        .block(BlockId::Height(height(1001)))
        .await
        .expect("serviceable")
        .expect("covered")
        .chainwork
        .expect("the head answered, rebased");

    assert_eq!(
        recent,
        finalised
            .accumulate(SingleBlockWork::new(
                core::num::NonZeroU128::new(1).expect("non-zero"),
            ))
            .expect("one unit cannot overflow"),
        "one block of work apart, with no step at the seam",
    );
}

/// With a hole below the window, the anchor's chainwork is unknown, so a
/// head-answered block reports none rather than a number measured from the
/// wrong place.
#[tokio::test]
async fn a_head_block_has_no_chainwork_while_the_store_is_behind_the_anchor() {
    let chain = chain();

    let block = syncing(&chain)
        .snapshot()
        .block(BlockId::Height(height(1150)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(block.chainwork, None);
}

/// A window floored at genesis has no anchor, so its work is already absolute
/// and needs no store read to serve.
#[tokio::test]
async fn a_window_floored_at_genesis_needs_no_anchor() {
    let chain = chain();
    // An empty store, so nothing but the missing anchor could supply this.
    let composer = view(&chain, 0, 0, 1200);

    let block = composer
        .snapshot()
        .block(BlockId::Height(height(500)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(block.chainwork, Some(chainwork_at(500)));
}

/// A block the validator answered for never carries chainwork, even where the
/// store could supply an anchor: the validator reports no work through this
/// path, so there is nothing to rebase.
#[tokio::test]
async fn a_source_answered_block_never_carries_chainwork() {
    let chain = chain();

    // Store to 100, window 1100..=1200: 500 is in the hole, and the validator
    // is the only provider that covers it.
    let block = syncing(&chain)
        .snapshot()
        .block(BlockId::Height(height(500)))
        .await
        .expect("serviceable")
        .expect("the validator fills it");

    assert_eq!(block.chainwork, None);
}

/// A streamed run is rebased the same way a point read is.
///
/// The batch path resolves the anchor once for the whole run rather than per
/// block, so it is a separate path through the same arithmetic and can drift
/// from the point read independently.
#[tokio::test]
async fn a_streamed_run_is_rebased_like_a_point_read() {
    let chain = chain();
    let blocks: Vec<_> = caught_up(&chain)
        .snapshot()
        .stream_blocks(height(999), height(1003))
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    let chainwork: Vec<_> = blocks.iter().map(|block| block.chainwork).collect();
    assert_eq!(
        chainwork,
        (999..=1003)
            .map(|h| Some(chainwork_at(h)))
            .collect::<Vec<_>>(),
        "every block, store-answered or head-answered, on one continuous scale",
    );
}
