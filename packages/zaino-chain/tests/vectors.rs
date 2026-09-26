//! The composition, against a real chain.
//!
//! The programmatic `Chain` in `zaino-chain::testing` gives exact control over
//! coverage shapes — an empty store, a hole at a chosen height — which a fixed
//! chain cannot express. What it cannot prove is that the stitched output is
//! *correct*: its blocks are synthetic, so a projection bug that mangled real
//! transactions would go unnoticed.
//!
//! These read the checked-in regtest vectors — the same chain
//! `zaino-chain-store-zainodb` builds its finalised-state suite from — and
//! assert that a range spanning store, hole and window returns the blocks the
//! vectors say, in order.

use std::sync::Arc;

use futures::TryStreamExt as _;
use zaino_chain::testing::{height, Chain, FakeHead, FakeSource, FakeStore};
use zaino_chain::{
    BlockId, BlockRead as _, ChainViewComposer, ChainViewConfig, ChainViewSnapshot as _,
    CompactBlockRead as _,
};
use zaino_chain_store::PoolFilter;
use zaino_primitives::types::{ChainMetadata, TreeSize};

/// The vector chain, as domain blocks.
///
/// Skipped rather than failed when the vectors are absent: they are checked in,
/// but a consumer vendoring this crate without them should not see a red suite
/// for a file it does not have.
fn vector_chain() -> Option<Chain> {
    let blocks = zaino_chain_store_zainodb::tests::vectors::load_vector_blocks().ok()?;

    let converted: Vec<_> = blocks
        .iter()
        .map(|vector| {
            zaino_convert_zebra::block_from_zebra(
                &vector.zebra_block,
                ChainMetadata {
                    sapling_tree_size: TreeSize::try_from(vector.sapling_tree_size)
                        .expect("vector tree sizes fit u32"),
                    orchard_tree_size: TreeSize::try_from(vector.orchard_tree_size)
                        .expect("vector tree sizes fit u32"),
                    ironwood_tree_size: TreeSize::ZERO,
                },
            )
            .expect("a checked-in vector block converts")
        })
        .collect();

    Some(Chain::from_blocks(converted))
}

/// A range spanning store, hole and window returns the real chain, in order.
///
/// The end-to-end proof: every provider contributes, and what comes back
/// matches the vectors block for block. A stitching bug that dropped, repeated
/// or reordered a segment boundary shows up here and nowhere else.
#[tokio::test]
async fn a_stitched_range_matches_the_vector_chain() {
    let Some(chain) = vector_chain() else {
        eprintln!("vector chain unavailable; skipping");
        return;
    };
    let tip = chain.tip();
    assert!(tip >= 20, "the vector chain is too short to span providers");

    // Coverage chosen so all three providers contribute: the store holds the
    // first fifth, the window the last fifth, and the validator the middle.
    let store_top = tip / 5;
    let head_floor = tip - (tip / 5);

    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, store_top),
        FakeHead::covering(&chain, head_floor, tip),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    assert_eq!(
        snapshot.serviceable_range().gap_from,
        Some(height(store_top + 1)),
        "the arrangement should leave a hole for the validator to fill"
    );

    let streamed: Vec<_> = snapshot
        .stream_compact(height(0), height(tip), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(
        streamed.len() as u32,
        tip + 1,
        "the stream should cover the whole chain"
    );

    for (index, block) in streamed.iter().enumerate() {
        let expected = chain
            .block(index as u32)
            .expect("the vector chain has this height");
        assert_eq!(block.height, index as u32, "out of order at {index}");
        assert_eq!(
            block.hash, expected.header.hash,
            "wrong block at height {index}"
        );
        assert_eq!(
            block.prev_hash, expected.header.prev_hash,
            "wrong parent at height {index}"
        );
    }
}

/// Real transactions survive the projection, whichever provider supplied them.
///
/// The vectors carry blocks with actual shielded and transparent activity, so
/// this is where a mangled projection would surface — the synthetic chain's
/// blocks are empty and cannot catch it.
#[tokio::test]
async fn transactions_survive_every_provider() {
    let Some(chain) = vector_chain() else {
        eprintln!("vector chain unavailable; skipping");
        return;
    };
    let tip = chain.tip();

    // The busiest height, excluding the ends so every arrangement below is
    // constructible: the validator's needs room for a window above it.
    let busiest = (1..tip)
        .max_by_key(|h| {
            chain
                .block(*h)
                .map(|block| block.transactions.len())
                .unwrap_or(0)
        })
        .expect("a non-empty chain");
    let expected = chain.block(busiest).expect("in range").transactions.len();
    assert!(expected > 0, "the vector chain has no transactions");

    // The same height, answered by each provider in turn. Coverage is chosen
    // so exactly one of them can serve it, which is what makes the comparison
    // a statement about that provider.
    let arrangements = [
        // The store covers it, and wins the overlap regardless of the window.
        ("store", busiest, tip, tip),
        // The window starts at it.
        ("head", 0, busiest, tip),
        // Neither covers it: the store stops below, the window starts above.
        ("validator", 0, busiest + 1, tip),
    ];

    for (who, store_top, head_floor, head_tip) in arrangements {
        if head_floor > head_tip {
            continue;
        }
        let composer = ChainViewComposer::new(
            FakeStore::covering(&chain, store_top),
            FakeHead::covering(&chain, head_floor, head_tip),
            Arc::new(FakeSource::over(&chain)),
            ChainViewConfig::default(),
        );

        let block = composer
            .snapshot()
            .compact_block(height(busiest), PoolFilter::all())
            .await
            .unwrap_or_else(|error| panic!("{who} could not serve height {busiest}: {error}"))
            .unwrap_or_else(|| panic!("{who} had no block at height {busiest}"));

        assert_eq!(
            block.transactions.len(),
            expected,
            "{who} lost transactions at height {busiest}"
        );
        assert_eq!(
            block.hash,
            chain.block(busiest).expect("in range").header.hash
        );
    }
}

/// An indexed block from the store carries chainwork; one from elsewhere does
/// not.
#[tokio::test]
async fn chainwork_marks_which_provider_answered() {
    let Some(chain) = vector_chain() else {
        eprintln!("vector chain unavailable; skipping");
        return;
    };
    let tip = chain.tip();
    let store_top = tip / 5;

    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, store_top),
        FakeHead::covering(&chain, tip - (tip / 5), tip),
        Arc::new(FakeSource::over(&chain)),
        ChainViewConfig::default(),
    );
    let snapshot = composer.snapshot();

    let below = snapshot
        .block(BlockId::Height(height(store_top)))
        .await
        .expect("serviceable")
        .expect("the store covers it");
    assert!(below.chainwork.is_some());

    let above = snapshot
        .block(BlockId::Height(height(store_top + 1)))
        .await
        .expect("serviceable")
        .expect("the validator fills it");
    assert!(
        above.chainwork.is_none(),
        "cumulative work cannot be known above the hole"
    );
}
