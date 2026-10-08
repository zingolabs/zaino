//! Tests for the legacy index's finality-seam watermark bridge.
//!
//! The bridge ([`spawn_confirmed_watermark_bridge`]) is the legacy path's own
//! side of the finality seam: it holds the seam's durable half and advances it
//! from the finalised store's committed watermark, authorised by the horizon the
//! chain head publishes. These tests drive the two seam halves directly — a mock
//! store-watermark channel standing in for the finalised store and a
//! [`ReorgHorizon`] standing in for the chain head — and assert the ratchet
//! turns, and that a commit past the horizon is refused rather than clamped.
//!
//! [`spawn_confirmed_watermark_bridge`]: crate::chain_index::spawn_confirmed_watermark_bridge

use tokio::sync::watch;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use zaino_chain_store::{Provenance, StoreWatermark};
use zaino_finality::{ReorgHorizon, Seam, DEFAULT_RETENTION_MARGIN};
use zaino_primitives::types::{BlockHash, BlockRef, Height};

use crate::chain_index::spawn_confirmed_watermark_bridge;

fn height(n: u32) -> Height {
    Height::try_from(n).expect("test heights are in range")
}

fn hash(n: u8) -> BlockHash {
    BlockHash::from([n; 32])
}

/// A `StoreWatermark` the finalised store would publish once it has durably
/// committed up to `n`.
fn durable_at(n: u32) -> StoreWatermark {
    StoreWatermark {
        tip: Some(BlockRef {
            hash: hash(0),
            height: height(n),
        }),
        provenance: Provenance::Durable,
    }
}

/// Polls the horizon's view of the durable watermark until it reaches
/// `expected`, failing if the bridge does not advance it within the timeout.
async fn wait_for_durable(horizon: &ReorgHorizon, expected: Height) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if horizon.durable().map(|committed| committed.height()) == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("bridge should advance the durable watermark within the timeout");
}

/// The ratchet turns: the chain head publishes a horizon and the bridge advances
/// the durable watermark against it, up to the height the finalised store has
/// committed.
#[tokio::test]
async fn bridge_advances_watermark_against_published_horizon() {
    // `reorg_depth = 0` so the horizon is the tip itself, keeping the arithmetic
    // in the test obvious: a tip of 10 is a horizon of 10.
    let (mut horizon, watermark) = Seam::new(0, DEFAULT_RETENTION_MARGIN).split();

    // The finalised store starts empty and passing through, as it does at boot.
    let (store_tx, store_rx) = watch::channel(StoreWatermark::empty());
    let cancel = CancellationToken::new();
    spawn_confirmed_watermark_bridge(store_rx, watermark, cancel.clone());

    // The chain head publishes a horizon (tip 10 -> r = 10), then the store
    // durably commits up to height 5 — below the horizon, so a legal advance.
    horizon
        .advance(height(10), hash(10))
        .expect("first horizon is legal");
    store_tx
        .send(durable_at(5))
        .expect("bridge holds the receiver");

    wait_for_durable(&horizon, height(5)).await;

    cancel.cancel();
}

/// A commit past the horizon is refused, not clamped: the finalised store builds
/// on its own schedule and can outrun the horizon, and when it does the bridge
/// leaves the watermark where it was rather than committing inside the reorg
/// window. A later legal commit still turns the ratchet, proving the bridge
/// stayed live through the refusal.
#[tokio::test]
async fn bridge_refuses_commit_past_horizon() {
    let (mut horizon, watermark) = Seam::new(0, DEFAULT_RETENTION_MARGIN).split();

    let (store_tx, store_rx) = watch::channel(StoreWatermark::empty());
    let cancel = CancellationToken::new();
    spawn_confirmed_watermark_bridge(store_rx, watermark, cancel.clone());

    // Horizon at 5, but the store reports a durable commit at 10 — past the
    // horizon. The seam rejects it (`WatermarkPastHorizon`), so the watermark
    // must stay unpublished.
    horizon
        .advance(height(5), hash(5))
        .expect("first horizon is legal");
    store_tx
        .send(durable_at(10))
        .expect("bridge holds the receiver");

    // Give the bridge time to process and reject the over-horizon commit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        horizon.durable(),
        None,
        "a commit past the horizon must not advance the watermark"
    );

    // Positive control: raise the horizon above the store and send a fresh
    // commit. The ratchet turns, which proves the bridge was alive and the
    // earlier `None` was a genuine refusal rather than latency.
    horizon
        .advance(height(10), hash(10))
        .expect("raising the horizon is legal");
    store_tx
        .send(durable_at(8))
        .expect("bridge holds the receiver");

    wait_for_durable(&horizon, height(8)).await;

    cancel.cancel();
}
