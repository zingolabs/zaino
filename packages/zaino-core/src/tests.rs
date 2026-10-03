//! Engine-engine tests: the light-serve acceptance gate, the per-capability
//! routing tests under `LightWalletRouting`, the manifest derivation, and the seam
//! split a local merge relies on.
//!
//! Exercised entirely with in-crate mocks — stub views for the composed chain
//! and a [`MockChain`] for the validator source, wrapped in the
//! [`ValidatorClient`] decorator exactly as the root injects it. No cluster, no
//! validator; the routing is deterministic.

use crate::routing::LightWalletRouting;
use crate::testing::{StubNonFinalised, stub_compact_block};
use futures::stream::StreamExt;
use zaino_service::error::{AddressReadError, BroadcastRejection, TreestateReadError};
use zaino_service::testing::{MockChain as MockService, MockIndexerService};
use zaino_service::{
    AddressRead, Broadcast, MempoolContent, MempoolSubscribe, RawTransactionRead, Serviceable,
    TakeSnapshot, TreestateRead,
};
use zaino_source::mock::MockChain;
use zaino_source::{RetryPolicy, SendRawTransactionError, ValidatorClient};

use zaino_primitives::types::{
    BlockRef, Height, HeightRange, RawTransaction, ShieldedPool, TransactionId,
    TransactionLocation, TransparentAddress,
};
use zaino_service::{Answerable, Capability};

use crate::Engine;
use crate::engine::split_at_seam;

type LightEngine =
    Engine<StubNonFinalised, StubNonFinalised, ValidatorClient<MockChain>, LightWalletRouting>;

fn height(h: u32) -> Height {
    Height::try_from(h).expect("valid height")
}

fn range(start: u32, end: u32) -> HeightRange {
    HeightRange {
        start: height(start),
        end: height(end),
    }
}

// The engine consumes the *canonical* (resilient) source, so the mock is wrapped
// in the ValidatorClient decorator — exactly how the root injects it.
fn engine_with(source: MockChain) -> LightEngine {
    Engine::new(
        StubNonFinalised::empty(),
        StubNonFinalised::empty(),
        ValidatorClient::new(source, RetryPolicy::default()),
    )
}

/// Review Focus 1: an unreachable validator is an error, never a zero-valued
/// success. The explorer's metric warmers keep their previous cache on an error
/// and would otherwise cache a wrong value for 15 seconds.
///
/// `ValidatorClient` retries, so a single injected failure must be terminal — a
/// one-attempt policy — otherwise attempt two succeeds and the test passes
/// vacuously, defeating the guard it exists to be.
#[tokio::test]
async fn an_unreachable_validator_errors_rather_than_answering_zero() {
    use zaino_service::NodeStatusRead;
    use zaino_source::FailureMode;

    let one_attempt = RetryPolicy {
        max_attempts: 1,
        ..RetryPolicy::default()
    };
    let engine: LightEngine = Engine::new(
        StubNonFinalised::empty(),
        StubNonFinalised::empty(),
        ValidatorClient::new(
            MockChain::new().fail_next(1, FailureMode::Connection),
            one_attempt,
        ),
    );
    assert!(engine.network_sol_ps(None, None).await.is_err());
}

#[tokio::test]
async fn broadcast_relays_to_the_source_and_returns_its_txid() {
    let engine = engine_with(MockChain::new());
    let raw = vec![7u8; 32];
    let txid = engine.broadcast(raw.clone()).await.expect("accepted");
    // The mock echoes the submitted bytes as the id, proving the exact
    // transaction reached the source's send port.
    let mut expected = [0u8; 32];
    expected.copy_from_slice(&raw);
    assert_eq!(txid, TransactionId::from(expected));
}

#[tokio::test]
async fn broadcast_maps_a_validator_rejection_to_invalid() {
    let engine = engine_with(
        MockChain::new().reject_send(SendRawTransactionError::Rejected("bad script".into())),
    );
    match engine.broadcast(vec![1, 2, 3]).await {
        Err(BroadcastRejection::Invalid(reason)) => assert_eq!(reason, "bad script"),
        other => panic!("expected an Invalid rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn broadcast_maps_malformed_bytes_to_malformed() {
    let engine = engine_with(
        MockChain::new().reject_send(SendRawTransactionError::Malformed("not a tx".into())),
    );
    match engine.broadcast(vec![0xff]).await {
        Err(BroadcastRejection::Malformed(reason)) => assert_eq!(reason, "not a tx"),
        other => panic!("expected a Malformed rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn mempool_raw_transaction_maps_a_missing_txid_to_none() {
    // The mock reports the txid absent (the listing/fetch race); the passthrough
    // maps that domain miss to `Ok(None)`, never a failure.
    let engine = engine_with(MockChain::new());
    let answer = engine
        .mempool_raw_transaction(TransactionId::from([9u8; 32]))
        .await
        .expect("a domain miss is a served None, not an error");
    assert!(answer.is_none());
}

#[tokio::test]
async fn subscribe_mempool_over_an_empty_mempool_yields_nothing() {
    let engine = engine_with(MockChain::new());
    let listing: Vec<_> = engine.subscribe_mempool().collect().await;
    assert!(listing.is_empty());
}

#[tokio::test]
async fn mempool_compact_transaction_maps_a_missing_txid_to_none() {
    let engine = engine_with(MockChain::new());
    let answer = engine
        .mempool_compact_transaction(TransactionId::from([9u8; 32]))
        .await
        .expect("a domain miss is a served None, not an error");
    assert!(answer.is_none());
}

#[tokio::test]
async fn mempool_listing_over_an_empty_mempool_is_empty_and_consistent() {
    use zaino_service::MempoolListing;
    // A validator that exposes an empty mempool is served as empty across all
    // three questions, and the summary agrees with the listings.
    let engine = engine_with(MockChain::new());
    assert!(engine.mempool_txids().await.expect("txids").is_empty());
    assert!(engine.mempool_entries().await.expect("entries").is_empty());
    let summary = engine.mempool_summary().await.expect("summary");
    assert_eq!(summary.size, 0);
    assert_eq!(summary.bytes, 0);
}

#[tokio::test]
async fn an_unreachable_validator_fails_the_mempool_summary() {
    use zaino_service::MempoolListing;
    use zaino_source::FailureMode;
    // Review Focus 1 on the mempool-info path: a transport failure is an error,
    // never a zero-valued success the explorer's warmer would cache. The policy
    // is single-attempt so the one injected failure is terminal.
    let one_attempt = RetryPolicy {
        max_attempts: 1,
        ..RetryPolicy::default()
    };
    let engine: LightEngine = Engine::new(
        StubNonFinalised::empty(),
        StubNonFinalised::empty(),
        ValidatorClient::new(
            MockChain::new().fail_next(1, FailureMode::Connection),
            one_attempt,
        ),
    );
    assert!(engine.mempool_summary().await.is_err());
}

// The acceptance gate for the full light-wallet read-set: under `LightWalletRouting`
// the composed engine serves every read `LightWalletService` demands — none
// reporting itself `NotServiceable`. The per-cap tests below pin each
// capability's placement.
#[tokio::test]
async fn light_serve_conformance_over_a_provisioned_source() {
    let engine = engine_with(MockChain::new());
    zaino_service::conformance::assert_light_wallet_conformance(&engine).await;
}

#[tokio::test]
async fn address_reads_are_remote_under_light_routing() {
    // No rejection seeded: the mock answers empty (no-match) results. An `Ok` —
    // not a `NotServiceable` stub — proves each address read routes to the
    // passthrough provider, as `LightWalletRouting::Address = Passthrough` says.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("t1ExampleProbeAddress0000000000000000".to_string());
    AddressRead::balance(&snapshot, &addr, range(0, 10))
        .await
        .expect("balance served");
    assert!(
        AddressRead::unspent_outpoints(&snapshot, &addr)
            .await
            .expect("utxos served")
            .is_empty()
    );
    assert!(
        AddressRead::tx_ids(&snapshot, &addr, range(0, 10))
            .await
            .expect("txids served")
            .is_empty()
    );
    assert!(
        AddressRead::deltas(&snapshot, &addr, range(0, 10))
            .await
            .expect("deltas served")
            .is_empty()
    );
}

/// Under passthrough routing the validator's `getaddresstxids` carries no
/// heights, so every pair's height is `None` — the honest "location unknown",
/// never a fabricated value. The source is scripted with two txids so the
/// assertion is not vacuous.
#[tokio::test]
async fn passthrough_tx_ids_report_no_height() {
    use zaino_primitives::types::TransactionId;
    let txids = vec![
        TransactionId::from([0x11; 32]),
        TransactionId::from([0x22; 32]),
    ];
    let engine = engine_with(MockChain::new().with_address_txids(txids.clone()));
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("t1ExampleProbeAddress0000000000000000".to_string());
    let located = AddressRead::tx_ids(&snapshot, &addr, range(0, 10))
        .await
        .expect("txids served");
    let returned: Vec<TransactionId> = located.iter().map(|(_, txid)| *txid).collect();
    assert_eq!(returned, txids, "the validator's txids pass through");
    assert!(
        located.iter().all(|(height, _)| height.is_none()),
        "a passthrough source reports no height, so each is None, not a fabricated value"
    );
}

#[tokio::test]
async fn address_reads_map_an_invalid_address_to_fatal() {
    let engine = engine_with(MockChain::new().reject_addresses("bad t-addr"));
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("bogus".to_string());
    match AddressRead::balance(&snapshot, &addr, range(0, 10)).await {
        Err(AddressReadError::Fatal(msg)) => {
            assert!(msg.contains("invalid address"), "got: {msg}")
        }
        other => panic!("expected a definitive invalid-address failure, got {other:?}"),
    }
}

#[tokio::test]
async fn raw_transaction_passes_through_the_bytes_and_location() {
    let bytes = vec![0xab, 0xcd, 0xef];
    let engine = engine_with(
        MockChain::new()
            .respond_transaction(bytes.clone(), TransactionLocation::BestChain(height(9))),
    );
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let got = RawTransactionRead::raw_transaction(&snapshot, TransactionId::from([1u8; 32]))
        .await
        .expect("served");
    assert_eq!(
        got,
        Some(RawTransaction {
            data: bytes,
            location: TransactionLocation::BestChain(height(9)),
        })
    );
}

#[tokio::test]
async fn raw_transaction_maps_a_missing_txid_to_none() {
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let got = RawTransactionRead::raw_transaction(&snapshot, TransactionId::from([2u8; 32]))
        .await
        .expect("served");
    assert_eq!(got, None);
}

#[tokio::test]
async fn subtree_roots_are_remote_under_light_routing() {
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let roots = TreestateRead::subtree_roots(&snapshot, ShieldedPool::Sapling, 0, None)
        .await
        .expect("subtree roots served");
    assert!(roots.is_empty());
}

#[tokio::test]
async fn compact_block_nullifiers_are_served_locally() {
    use zaino_primitives::types::BlockSelector;
    use zaino_service::CompactNullifierRead;
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let got =
        CompactNullifierRead::compact_block_nullifiers(&snapshot, BlockSelector::Height(height(0)))
            .await
            .expect("served");
    assert!(got.is_none());
}

#[tokio::test]
async fn treestate_is_remote_under_light_routing() {
    // No treestate seeded: the mock answers HeightNotFound, which the passthrough
    // provider maps to a definitive read failure. Proves treestate routes to the
    // passthrough provider, as `LightWalletRouting::Treestate = Passthrough` says.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    match TreestateRead::treestate(&snapshot, height(5)).await {
        Err(TreestateReadError::Fatal(msg)) => {
            assert!(msg.contains("no treestate at height"), "got: {msg}")
        }
        other => panic!("expected a definitive miss, got {other:?}"),
    }
}

// --- the manifest follows the routing ----------------------------------------

/// A finalised side that derives a real manifest: the service mock, whose
/// manifest is "to the tip" once it has one.
fn engine_over_a_serviceable_store(
    tip: Option<u32>,
) -> Engine<MockIndexerService, StubNonFinalised, ValidatorClient<MockChain>, LightWalletRouting> {
    let fs = MockIndexerService::new(MockService {
        tip: tip.map(|h| BlockRef {
            height: height(h),
            hash: [0u8; 32].into(),
        }),
        ..MockService::default()
    });
    Engine::new(
        fs,
        StubNonFinalised::empty(),
        ValidatorClient::new(MockChain::new(), RetryPolicy::default()),
    )
}

#[test]
fn the_manifest_is_derived_from_the_routing_and_the_store() {
    let manifest = engine_over_a_serviceable_store(Some(42)).serviceability();

    // Local: whatever the finalised store says.
    assert_eq!(
        manifest.get(Capability::Blocks),
        Answerable::ToHeight(height(42))
    );
    // Passthrough: live, the validator answers.
    assert_eq!(manifest.get(Capability::AddressHistory), Answerable::Live);
    assert_eq!(manifest.get(Capability::Treestate), Answerable::Live);
    assert_eq!(manifest.get(Capability::RawTransaction), Answerable::Live);
    assert_eq!(manifest.get(Capability::Broadcast), Answerable::Live);
    // Withheld: absent, whatever the providers could answer.
    assert_eq!(manifest.get(Capability::SpendStatus), Answerable::Absent);
    assert_eq!(
        manifest.get(Capability::TransactionLocation),
        Answerable::Absent
    );
}

#[test]
fn a_store_with_no_progress_makes_local_capabilities_not_yet() {
    let manifest = engine_over_a_serviceable_store(None).serviceability();
    assert_eq!(manifest.get(Capability::Blocks), Answerable::NotYet);
    // Passthrough and withheld are unaffected by the store's progress.
    assert_eq!(manifest.get(Capability::Treestate), Answerable::Live);
    assert_eq!(manifest.get(Capability::SpendStatus), Answerable::Absent);
}

// --- the seam split a local merge relies on ----------------------------------

/// A finalised side covering `[0, tip]` and an empty head.
async fn pinned_with_finalised_tip(
    tip: u32,
) -> crate::chain_view::ChainViewSnapshot<StubNonFinalised, StubNonFinalised> {
    let blocks = (0..=tip)
        .map(|h| stub_compact_block(h, 1))
        .collect::<Vec<_>>();
    crate::chain_view::ChainView::new(
        StubNonFinalised::from_blocks(blocks),
        StubNonFinalised::empty(),
    )
    .snapshot()
    .await
    .expect("snapshot")
}

#[tokio::test]
async fn a_range_splits_at_the_watermark() {
    let local = pinned_with_finalised_tip(10).await;
    // Straddling: `[5, 10]` is the store's, `[11, 20]` the head's. Both halves
    // are inclusive, like the range they split.
    assert_eq!(
        split_at_seam(&local, range(5, 20)),
        (Some(range(5, 10)), Some(range(11, 20)))
    );
    // Entirely below the seam.
    assert_eq!(
        split_at_seam(&local, range(0, 5)),
        (Some(range(0, 5)), None)
    );
    // Entirely above it.
    assert_eq!(
        split_at_seam(&local, range(11, 20)),
        (None, Some(range(11, 20)))
    );
    // The watermark height itself is the store's; the one above it is the
    // head's, so this straddles by a single height on each side.
    assert_eq!(
        split_at_seam(&local, range(8, 11)),
        (Some(range(8, 10)), Some(range(11, 11)))
    );
    // One height, at the watermark: the store's, and not dropped.
    assert_eq!(
        split_at_seam(&local, range(10, 10)),
        (Some(range(10, 10)), None)
    );
    // One height, just above it: the head's.
    assert_eq!(
        split_at_seam(&local, range(11, 11)),
        (None, Some(range(11, 11)))
    );
    // A range naming no height: start above end.
    assert_eq!(split_at_seam(&local, range(8, 7)), (None, None));
}

#[tokio::test]
async fn with_no_watermark_the_whole_range_is_the_heads() {
    let local =
        crate::chain_view::ChainView::new(StubNonFinalised::empty(), StubNonFinalised::empty())
            .snapshot()
            .await
            .expect("snapshot");
    assert_eq!(
        split_at_seam(&local, range(3, 9)),
        (None, Some(range(3, 9)))
    );
}

// --- full block reads: always passthrough, except the tip --------------------
//
// `BlockRead` routes a `BlockSelector` to the by-height or by-hash source port
// and reads the header off the block it already has; the tip is read locally off
// the pinned view. A domain miss is `Ok(None)`; an unreachable validator errors.
mod block_reads {
    use super::*;
    use zaino_primitives::types::{BlockHash, BlockSelector};
    use zaino_service::BlockRead;
    use zaino_service::error::{BlockReadError, ReadError};
    use zaino_source::FailureMode;
    use zaino_source::mock::test_block;

    /// A single-attempt policy: one injected failure is terminal, so an error
    /// test cannot be masked by the default policy's retries.
    fn single_attempt() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// An engine with an empty local view (no pinned tip) over `source`.
    fn engine_single_attempt(source: MockChain) -> LightEngine {
        Engine::new(
            StubNonFinalised::empty(),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, single_attempt()),
        )
    }

    /// An engine whose local view is pinned to a tip at `tip`, over `source` and
    /// `policy`. `stream_blocks` clamps to the local tip; the passthrough block
    /// reads draw their blocks from `source`.
    fn engine_with_tip(tip: u32, source: MockChain, policy: RetryPolicy) -> LightEngine {
        Engine::new(
            StubNonFinalised::from_blocks((0..=tip).map(|h| stub_compact_block(h, 1)).collect()),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, policy),
        )
    }

    /// A validator holding one block at each of `heights`.
    fn source_with_blocks(heights: impl IntoIterator<Item = u32>) -> MockChain {
        let mut mock = MockChain::new();
        for h in heights {
            mock = mock.with_block(test_block(h, u8::try_from(h + 1).expect("fits in u8")));
        }
        mock
    }

    #[tokio::test]
    async fn block_by_height_returns_the_scripted_block() {
        let engine = engine_with(MockChain::new().with_block(test_block(7, 7)));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let block = BlockRead::block(&snapshot, BlockSelector::Height(height(7)))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(block.header.height, height(7));
        assert_eq!(block.header.hash, BlockHash::from([7u8; 32]));
    }

    #[tokio::test]
    async fn block_by_hash_returns_the_same_block() {
        let engine = engine_with(MockChain::new().with_block(test_block(7, 7)));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let by_height = BlockRead::block(&snapshot, BlockSelector::Height(height(7)))
            .await
            .expect("served")
            .expect("present");
        let by_hash = BlockRead::block(&snapshot, BlockSelector::Hash(BlockHash::from([7u8; 32])))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(by_height.header, by_hash.header);
    }

    #[tokio::test]
    async fn block_header_returns_the_named_blocks_header() {
        let engine = engine_with(MockChain::new().with_block(test_block(7, 7)));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let header = BlockRead::block_header(&snapshot, BlockSelector::Height(height(7)))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(header.height, height(7));
        assert_eq!(header.hash, BlockHash::from([7u8; 32]));
    }

    #[tokio::test]
    async fn block_height_resolves_a_hash_to_its_height() {
        let engine = engine_with(MockChain::new().with_block(test_block(7, 7)));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let resolved = BlockRead::block_height(&snapshot, BlockHash::from([7u8; 32]))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(resolved, height(7));
    }

    #[tokio::test]
    async fn an_unknown_height_is_a_served_none_not_an_error() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let answer = BlockRead::block(&snapshot, BlockSelector::Height(height(99)))
            .await
            .expect("a domain miss is a served None, not an error");
        assert!(answer.is_none());
    }

    #[tokio::test]
    async fn an_unknown_hash_is_a_served_none() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let block = BlockRead::block(&snapshot, BlockSelector::Hash(BlockHash::from([3u8; 32])))
            .await
            .expect("a domain miss is a served None, not an error");
        assert!(block.is_none());
        let resolved = BlockRead::block_height(&snapshot, BlockHash::from([3u8; 32]))
            .await
            .expect("a domain miss is a served None, not an error");
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn stream_blocks_yields_every_block_in_the_inclusive_range() {
        // Tip at 6 so `[3, 6]` is not clamped; the validator holds 3..=6.
        let engine = engine_with_tip(6, source_with_blocks(3..=6), RetryPolicy::default());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let served: Vec<u32> = BlockRead::stream_blocks(&snapshot, range(3, 6))
            .map(|block| u32::from(block.expect("served").header.height))
            .collect()
            .await;
        // Inclusive `[3, 6]` is four blocks, both endpoints included, ascending.
        assert_eq!(served, vec![3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn stream_blocks_clamps_a_range_past_the_tip() {
        // Tip at 3; the validator holds 0..=3. Asking `[1, 9]` covers only up to
        // the pinned tip, with no error for the above-tip tail.
        let engine = engine_with_tip(3, source_with_blocks(0..=3), RetryPolicy::default());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let heights: Vec<u32> = BlockRead::stream_blocks(&snapshot, range(1, 9))
            .map(|block| u32::from(block.expect("served, no error past the tip").header.height))
            .collect()
            .await;
        assert_eq!(heights, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn stream_blocks_entirely_above_the_tip_yields_nothing() {
        // Tip at 3; `[4, 6]` is wholly above it — the caller asked past the end.
        let engine = engine_with_tip(3, source_with_blocks(0..=6), RetryPolicy::default());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let items: Vec<_> = BlockRead::stream_blocks(&snapshot, range(4, 6))
            .collect()
            .await;
        assert!(
            items.is_empty(),
            "a range entirely above the tip yields nothing"
        );
    }

    #[tokio::test]
    async fn stream_blocks_surfaces_a_hole_below_the_tip() {
        // Tip at 4; the validator holds 2 and 4 but not 3. Asking `[2, 4]` yields
        // block 2, then an error naming the missing height 3, and ends: block 4
        // must NOT be served past the hole. This is the regression check on the
        // former silent skip.
        let engine = engine_with_tip(4, source_with_blocks([2, 4]), RetryPolicy::default());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let items: Vec<_> = BlockRead::stream_blocks(&snapshot, range(2, 4))
            .collect()
            .await;
        assert_eq!(items.len(), 2, "block 2, then the hole error, then end");
        assert_eq!(
            u32::from(items[0].as_ref().expect("block 2 served").header.height),
            2
        );
        match &items[1] {
            Err(ReadError::Transient(msg)) => {
                assert!(
                    msg.contains('3'),
                    "the error names the missing height: {msg}"
                )
            }
            other => panic!("a hole below the tip is a transient error, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_blocks_with_no_pinned_tip_is_not_serviceable() {
        // Empty local view: the snapshot is coherent against nothing, like `tip`.
        let engine = engine_with(source_with_blocks(0..=3));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let items: Vec<_> = BlockRead::stream_blocks(&snapshot, range(0, 3))
            .collect()
            .await;
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items.into_iter().next(),
            Some(Err(ReadError::NotServiceable(Capability::Blocks)))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_validator_errors_rather_than_missing() {
        let engine = engine_single_attempt(
            MockChain::new()
                .with_block(test_block(5, 5))
                .fail_next(1, FailureMode::Connection),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match BlockRead::block(&snapshot, BlockSelector::Height(height(5))).await {
            Err(BlockReadError::Transient(_)) => {}
            other => panic!("an unreachable validator must error, not answer None: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_blocks_stops_at_a_read_error() {
        // Tip at 2 so `[0, 2]` is unclamped; one terminal failure hits the first
        // height. A stream that continued past the error would still serve 1 and 2.
        let engine = engine_with_tip(
            2,
            source_with_blocks(0..=2).fail_next(1, FailureMode::Connection),
            single_attempt(),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let items: Vec<_> = BlockRead::stream_blocks(&snapshot, range(0, 2))
            .collect()
            .await;
        assert_eq!(
            items.len(),
            1,
            "the stream stops at the first error, not after it"
        );
        assert!(matches!(
            items.into_iter().next(),
            Some(Err(ReadError::Transient(_)))
        ));
    }

    #[tokio::test]
    async fn tip_returns_the_pinned_tip() {
        let engine = engine_with_tip(4, MockChain::new(), RetryPolicy::default());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let tip = BlockRead::tip(&snapshot)
            .await
            .expect("a pinned snapshot has a tip");
        assert_eq!(tip.height, height(4));
    }

    #[tokio::test]
    async fn tip_errors_when_there_is_no_pinned_tip() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match BlockRead::tip(&snapshot).await {
            Err(BlockReadError::NotServiceable(Capability::Blocks)) => {}
            other => panic!("an empty snapshot has no tip to serve: {other:?}"),
        }
    }
}

mod transaction_reads {
    use super::*;
    use zaino_primitives::types::Transaction;
    use zaino_service::error::TxReadError;
    use zaino_service::{TransactionRead, TxStatus};
    use zaino_source::FailureMode;

    /// A single-attempt policy: one injected failure is terminal, so an error
    /// test cannot be masked by the default policy's retries.
    fn single_attempt() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// An engine with an empty local view over `source`, single-attempt, so an
    /// injected transport failure is terminal.
    fn engine_single_attempt(source: MockChain) -> LightEngine {
        Engine::new(
            StubNonFinalised::empty(),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, single_attempt()),
        )
    }

    /// A decoded transaction whose txid is `[txid_byte; 32]` and whose pools are
    /// empty — enough to assert identity without building pool data.
    fn decoded_tx(txid_byte: u8) -> Transaction {
        Transaction {
            txid: TransactionId::from([txid_byte; 32]),
            transparent: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    #[tokio::test]
    async fn a_scripted_txid_returns_the_decoded_transaction() {
        let engine =
            engine_with(MockChain::new().respond_transaction_verbose(
                decoded_tx(7),
                TransactionLocation::BestChain(height(9)),
            ));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let tx = TransactionRead::transaction(&snapshot, TransactionId::from([7u8; 32]))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(tx.txid, TransactionId::from([7u8; 32]));
    }

    #[tokio::test]
    async fn an_unknown_txid_is_a_served_none_not_an_error() {
        // No scripted response: the validator answers NotFound, which is a domain
        // miss, not a transport failure.
        let engine = engine_with(MockChain::new());
        let answer = {
            let snapshot = engine.snapshot().await.expect("snapshot acquired");
            TransactionRead::transaction(&snapshot, TransactionId::from([3u8; 32]))
                .await
                .expect("a domain miss is a served None, not an error")
        };
        assert!(answer.is_none());
    }

    #[tokio::test]
    async fn transaction_status_maps_best_chain_to_mined() {
        let engine =
            engine_with(MockChain::new().respond_transaction_verbose(
                decoded_tx(1),
                TransactionLocation::BestChain(height(9)),
            ));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let status = TransactionRead::transaction_status(&snapshot, TransactionId::from([1u8; 32]))
            .await
            .expect("served");
        assert_eq!(status, TxStatus::Mined(height(9)));
    }

    #[tokio::test]
    async fn transaction_status_maps_non_best_chain_to_orphaned() {
        let engine = engine_with(
            MockChain::new()
                .respond_transaction_verbose(decoded_tx(1), TransactionLocation::NonBestChain),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let status = TransactionRead::transaction_status(&snapshot, TransactionId::from([1u8; 32]))
            .await
            .expect("served");
        assert_eq!(status, TxStatus::Orphaned);
    }

    #[tokio::test]
    async fn transaction_status_maps_mempool_to_unknown_not_orphaned() {
        // A mempool transaction is not mined and has not been reorged out.
        // Collapsing it to Orphaned is how a consumer wrongly concludes a pending
        // transaction failed, so this is the distinction the test exists to pin.
        let engine = engine_with(
            MockChain::new()
                .respond_transaction_verbose(decoded_tx(1), TransactionLocation::Mempool),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let status = TransactionRead::transaction_status(&snapshot, TransactionId::from([1u8; 32]))
            .await
            .expect("served");
        assert_eq!(status, TxStatus::Unknown);
        assert_ne!(status, TxStatus::Orphaned);
    }

    #[tokio::test]
    async fn transaction_status_of_an_absent_transaction_is_unknown() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let status = TransactionRead::transaction_status(&snapshot, TransactionId::from([3u8; 32]))
            .await
            .expect("an absent transaction is a served Unknown, not an error");
        assert_eq!(status, TxStatus::Unknown);
    }

    #[tokio::test]
    async fn an_unreachable_validator_errors_rather_than_missing() {
        // A scripted transaction is present, but the single-attempt transport
        // failure is terminal: the read must surface it, not report Ok(None).
        let engine = engine_single_attempt(
            MockChain::new()
                .respond_transaction_verbose(
                    decoded_tx(5),
                    TransactionLocation::BestChain(height(5)),
                )
                .fail_next(1, FailureMode::Connection),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match TransactionRead::transaction(&snapshot, TransactionId::from([5u8; 32])).await {
            Err(TxReadError::Transient(_)) => {}
            other => panic!("an unreachable validator must error, not answer None: {other:?}"),
        }
    }

    /// With `BlockRead` and `TransactionRead` both implemented, `EngineSnapshot`
    /// satisfies `NodeRpcReads`, so a node-RPC-routed engine satisfies
    /// `NodeRpcService` — the milestone this task gates. Compile-time only.
    #[test]
    fn the_engine_satisfies_node_rpc_service() {
        use crate::routing::NodeRpcRouting;
        use zaino_service::NodeRpcService;

        // The production passthrough node-RPC routing: address and treestate pass
        // through, spend status and transaction location withheld. One
        // definition, in `crate::routing`.
        fn assert_node_rpc<T: NodeRpcService>() {}
        assert_node_rpc::<
            Engine<
                MockIndexerService,
                MockIndexerService,
                ValidatorClient<MockChain>,
                NodeRpcRouting,
            >,
        >();
    }
}

// --- verbose block reads: always passthrough -------------------------------
//
// `BlockVerboseRead` relays the validator's verbose header/block, carrying the
// chain-position facts (confirmations, difficulty, chainwork, neighbouring
// hashes) the stored block cannot give. A domain miss is `Ok(None)`; an
// unreachable validator errors.
mod block_verbose_reads {
    use super::*;
    use zaino_primitives::types::{BlockHash, BlockSelector};
    use zaino_service::BlockVerboseRead;
    use zaino_service::error::BlockReadError;
    use zaino_source::FailureMode;
    use zaino_source::mock::{sample_block_header_verbose, sample_block_verbose};

    /// A single-attempt policy: one injected failure is terminal, so an error
    /// test cannot be masked by the default policy's retries.
    fn single_attempt() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// An engine with an empty local view over `source`, single-attempt, so an
    /// injected transport failure is terminal.
    fn engine_single_attempt(source: MockChain) -> LightEngine {
        Engine::new(
            StubNonFinalised::empty(),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, single_attempt()),
        )
    }

    #[tokio::test]
    async fn block_header_verbose_passes_the_validators_header_through() {
        let engine =
            engine_with(MockChain::new().with_block_header_verbose(sample_block_header_verbose()));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let header = BlockVerboseRead::block_header_verbose(&snapshot, BlockHash::from([1u8; 32]))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(header, sample_block_header_verbose());
    }

    /// A verbose block distinct from [`sample_block_verbose`] in an asserted
    /// field, so the by-height and by-hash arms carry different canned values and
    /// a swapped selector is caught rather than passing on a shared value.
    fn distinct_verbose() -> zaino_primitives::types::BlockVerbose {
        let mut block = sample_block_verbose();
        block.confirmations = 99;
        block
    }

    #[tokio::test]
    async fn block_verbose_by_height_passes_through() {
        let engine = engine_with(
            MockChain::new()
                .with_block_verbose(sample_block_verbose())
                .with_block_verbose_by_hash(distinct_verbose()),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let block = BlockVerboseRead::block_verbose(&snapshot, BlockSelector::Height(height(5)))
            .await
            .expect("served")
            .expect("present");
        // The by-height selector reads the by-height port, not the by-hash one.
        assert_eq!(block, sample_block_verbose());
        assert_ne!(block, distinct_verbose());
    }

    #[tokio::test]
    async fn block_verbose_by_hash_passes_through() {
        let engine = engine_with(
            MockChain::new()
                .with_block_verbose(sample_block_verbose())
                .with_block_verbose_by_hash(distinct_verbose()),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let block = BlockVerboseRead::block_verbose(
            &snapshot,
            BlockSelector::Hash(BlockHash::from([2u8; 32])),
        )
        .await
        .expect("served")
        .expect("present");
        // The by-hash selector reads the by-hash port, not the by-height one.
        assert_eq!(block, distinct_verbose());
        assert_ne!(block, sample_block_verbose());
    }

    #[tokio::test]
    async fn an_unknown_block_is_a_served_none_not_an_error() {
        // No scripted response: the validator answers a domain not-found, which
        // the passthrough maps to `Ok(None)`, never a failure.
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        assert!(
            BlockVerboseRead::block_header_verbose(&snapshot, BlockHash::from([9u8; 32]))
                .await
                .expect("a domain miss is a served None, not an error")
                .is_none()
        );
        assert!(
            BlockVerboseRead::block_verbose(&snapshot, BlockSelector::Height(height(99)))
                .await
                .expect("a domain miss is a served None, not an error")
                .is_none()
        );
        assert!(
            BlockVerboseRead::block_verbose(
                &snapshot,
                BlockSelector::Hash(BlockHash::from([9u8; 32]))
            )
            .await
            .expect("a domain miss is a served None, not an error")
            .is_none()
        );
    }

    #[tokio::test]
    async fn an_unreachable_validator_errors_rather_than_missing() {
        // A scripted header is present, but the single-attempt transport failure
        // is terminal: the read must surface it, not report Ok(None).
        let engine = engine_single_attempt(
            MockChain::new()
                .with_block_header_verbose(sample_block_header_verbose())
                .with_block_verbose(sample_block_verbose())
                .fail_next(1, FailureMode::Connection),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match BlockVerboseRead::block_header_verbose(&snapshot, BlockHash::from([1u8; 32])).await {
            Err(BlockReadError::Transient(_)) => {}
            other => panic!("an unreachable validator must error, not answer None: {other:?}"),
        }
    }

    #[tokio::test]
    async fn raw_block_by_height_passes_through() {
        // The two arms carry distinct bytes, so a by-height call that read the
        // by-hash value (a swapped selector) would fail here.
        let engine = engine_with(
            MockChain::new()
                .with_raw_block(vec![0xAA, 0xBB])
                .with_raw_block_by_hash(vec![0xCC, 0xDD]),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let bytes = BlockVerboseRead::raw_block(&snapshot, BlockSelector::Height(height(5)))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(bytes, vec![0xAA, 0xBB]);
    }

    #[tokio::test]
    async fn raw_block_by_hash_passes_through() {
        let engine = engine_with(
            MockChain::new()
                .with_raw_block(vec![0xAA, 0xBB])
                .with_raw_block_by_hash(vec![0xCC, 0xDD]),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let bytes =
            BlockVerboseRead::raw_block(&snapshot, BlockSelector::Hash(BlockHash::from([2u8; 32])))
                .await
                .expect("served")
                .expect("present");
        // The by-hash selector reads the by-hash port, not the by-height one.
        assert_eq!(bytes, vec![0xCC, 0xDD]);
    }

    #[tokio::test]
    async fn an_unknown_raw_block_is_a_served_none() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        assert!(
            BlockVerboseRead::raw_block(&snapshot, BlockSelector::Height(height(99)))
                .await
                .expect("a domain miss is a served None, not an error")
                .is_none()
        );
        assert!(
            BlockVerboseRead::raw_block(&snapshot, BlockSelector::Hash(BlockHash::from([9u8; 32])))
                .await
                .expect("a domain miss is a served None, not an error")
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unreachable_validator_errors_on_raw_block() {
        // A scripted block is present, but the single-attempt transport failure is
        // terminal: the read must surface it, not report Ok(None).
        let engine = engine_single_attempt(
            MockChain::new()
                .with_raw_block(vec![0xAA, 0xBB])
                .fail_next(1, FailureMode::Connection),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match BlockVerboseRead::raw_block(&snapshot, BlockSelector::Height(height(5))).await {
            Err(BlockReadError::Transient(_)) => {}
            other => panic!("an unreachable validator must error, not answer None: {other:?}"),
        }
    }
}

// --- block-hash reads: the getblockhashes timestamp-range selection ----------
//
// `BlockHashRead::block_hashes` drives the pure candidate-bracket search over the
// composed chain view's header reads, then filters the bracket by each block's
// actual time. Local over both tiers; a range beyond the tip is an empty list; a
// hole in the view is a typed `MissingHeader`.
mod block_hash_reads {
    use super::*;
    use zaino_primitives::types::CompactBlock;
    use zaino_service::BlockHashRead;
    use zaino_service::error::BlockHashReadError;

    /// The base timestamp the test chains build from.
    const BASE: u32 = 1_000_000;

    /// A compact block at `height` whose hash byte is `hash_byte` and whose time
    /// is `time`, so a returned [`BlockHashAt`](zaino_service::BlockHashAt)
    /// identifies both the block and the tier it was routed to.
    fn block_at(height: u32, hash_byte: u8, time: u32) -> CompactBlock {
        let mut block = stub_compact_block(height, hash_byte);
        block.time = time;
        block
    }

    /// An engine composing `fs` as the finalised tier and `nfs` as the
    /// non-finalised tier over an empty validator (the read is local, so the
    /// validator is never consulted).
    fn engine_over_tiers(fs: Vec<CompactBlock>, nfs: Vec<CompactBlock>) -> LightEngine {
        Engine::new(
            StubNonFinalised::from_blocks(fs),
            StubNonFinalised::from_blocks(nfs),
            ValidatorClient::new(MockChain::new(), RetryPolicy::default()),
        )
    }

    /// Strictly-increasing block times `BASE + h*600` for heights `0..n` — a
    /// consensus-valid chain (`nTime > MTP` holds, and the drift stays well under
    /// the 90-minute bound), with a monotonic median-time-past the search relies
    /// on.
    fn linear_times(n: u32) -> Vec<u32> {
        (0..n).map(|h| BASE + h * 600).collect()
    }

    /// The heights of a block-hash result, in result order.
    fn heights_of(result: &[zaino_service::BlockHashAt]) -> Vec<u32> {
        result.iter().map(|entry| u32::from(entry.height)).collect()
    }

    /// The leading hash byte of each result entry — its tier tag, in this suite.
    fn tags_of(result: &[zaino_service::BlockHashAt]) -> Vec<u8> {
        result
            .iter()
            .map(|entry| <[u8; 32]>::from(entry.hash)[0])
            .collect()
    }

    /// A chain split at watermark 8 with tip 12: the finalised tier holds
    /// `[0, 8]` (hash tags `0x10 + h`) and the non-finalised tier holds `[7, 12]`
    /// (tags `0x80 + h`). The overlap `[7, 8]` is on both tiers with different
    /// tags, so a height that mis-routes across the seam returns the wrong tag and
    /// fails the assertion.
    fn split_chain() -> LightEngine {
        let times = linear_times(13);
        let fs = (0..=8)
            .map(|h| {
                block_at(
                    h,
                    u8::try_from(0x10 + h).expect("tag fits u8"),
                    times[usize::try_from(h).expect("height fits usize")],
                )
            })
            .collect();
        let nfs = (7..=12)
            .map(|h| {
                block_at(
                    h,
                    u8::try_from(0x80 + h).expect("tag fits u8"),
                    times[usize::try_from(h).expect("height fits usize")],
                )
            })
            .collect();
        engine_over_tiers(fs, nfs)
    }

    #[tokio::test]
    async fn a_range_inside_the_finalised_tier() {
        let engine = split_chain();
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let times = linear_times(13);
        // `[times[2], times[5] + 1)` matches heights 2..=5, all below the watermark.
        let result = BlockHashRead::block_hashes(&snapshot, times[2], times[5] + 1)
            .await
            .expect("served");
        assert_eq!(heights_of(&result), vec![2, 3, 4, 5]);
        // Every hit carries a finalised-tier tag (`0x10 + h`), proving the read
        // routed below the seam.
        assert_eq!(tags_of(&result), vec![0x12, 0x13, 0x14, 0x15]);
    }

    #[tokio::test]
    async fn a_range_inside_the_non_finalised_tier() {
        let engine = split_chain();
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let times = linear_times(13);
        // `[times[10], times[12] + 1)` matches heights 10..=12, all above the seam.
        let result = BlockHashRead::block_hashes(&snapshot, times[10], times[12] + 1)
            .await
            .expect("served");
        assert_eq!(heights_of(&result), vec![10, 11, 12]);
        // Non-finalised-tier tags (`0x80 + h`), proving the read routed above the
        // seam.
        assert_eq!(tags_of(&result), vec![0x8A, 0x8B, 0x8C]);
    }

    #[tokio::test]
    async fn a_range_across_the_seam_returns_both_tiers_with_no_gap() {
        let engine = split_chain();
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let times = linear_times(13);
        // `[times[7], times[10] + 1)` spans the watermark (8): heights 7, 8 are the
        // finalised tier's and 9, 10 the non-finalised tier's.
        let result = BlockHashRead::block_hashes(&snapshot, times[7], times[10] + 1)
            .await
            .expect("served");
        assert_eq!(heights_of(&result), vec![7, 8, 9, 10]);
        // 7 and 8 carry finalised tags even though the NFS window overlaps them;
        // 9 and 10 carry non-finalised tags. No gap at the seam, no duplicate.
        assert_eq!(tags_of(&result), vec![0x17, 0x18, 0x89, 0x8A]);
    }

    #[tokio::test]
    async fn a_range_beyond_the_tip_is_empty() {
        let engine = split_chain();
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let times = linear_times(13);
        let after_tip = times[12] + 10_000;
        let result = BlockHashRead::block_hashes(&snapshot, after_tip, after_tip + 10_000)
            .await
            .expect("a range beyond the tip is a served empty list, not an error");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn an_out_of_order_block_is_returned_exactly_when_in_range() {
        // A consensus-valid chain whose height 13 carries a timestamp *below* its
        // predecessor's (height 12 spikes to the drift ceiling, 13 dips back). A
        // timestamp slice of the height axis would miss it; the candidate search
        // must return it when its own time is in range.
        let mut times = linear_times(12); // heights 0..=11 at BASE + h*600
        times.push(BASE + 9_000); // height 12: a spike (MTP(12)=BASE+3600, +5400 ceiling)
        times.push(BASE + 4_300); // height 13: a dip, below height 12's time
        assert!(
            times[13] < times[12],
            "the chain must actually be out of order"
        );

        let fs = (0..=13)
            .map(|h| {
                block_at(
                    h,
                    u8::try_from(0x10 + h).expect("tag fits u8"),
                    times[usize::try_from(h).expect("height fits usize")],
                )
            })
            .collect();
        let engine = engine_over_tiers(fs, Vec::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");

        // A one-wide range on the dipped block's own time selects exactly it, and
        // not the higher-timestamped spike at height 12.
        let result = BlockHashRead::block_hashes(&snapshot, times[13], times[13] + 1)
            .await
            .expect("served");
        assert_eq!(heights_of(&result), vec![13]);

        // The spike is independently addressable on its own (higher) time.
        let spike = BlockHashRead::block_hashes(&snapshot, times[12], times[12] + 1)
            .await
            .expect("served");
        assert_eq!(heights_of(&spike), vec![12]);
    }

    #[tokio::test]
    async fn results_are_ascending_by_time_then_by_hash() {
        // Heights 2 and 3 share a timestamp; their hash tags are out of height
        // order (3 < 2), so a correct sort returns height 3 before height 2.
        let times = [BASE, BASE + 600, BASE + 1_200, BASE + 1_200, BASE + 1_800];
        let tags = [0x40u8, 0x41, 0x30, 0x20, 0x44];
        let fs = (0u32..5)
            .map(|h| {
                let idx = usize::try_from(h).expect("height fits usize");
                block_at(h, tags[idx], times[idx])
            })
            .collect();
        let engine = engine_over_tiers(fs, Vec::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");

        let result = BlockHashRead::block_hashes(&snapshot, BASE, BASE + 1_800 + 1)
            .await
            .expect("served");
        // Ascending by time, then by hash: the B+1200 tie orders height 3 (tag
        // 0x20) before height 2 (tag 0x30).
        assert_eq!(heights_of(&result), vec![0, 1, 3, 2, 4]);
        // The (time, hash) pairs are non-decreasing across the whole result.
        for pair in result.windows(2) {
            assert!(
                (pair[0].time, pair[0].hash) <= (pair[1].time, pair[1].hash),
                "result is not sorted ascending by (time, hash)"
            );
        }
    }

    #[tokio::test]
    async fn a_hole_in_the_view_is_a_typed_missing_header() {
        // A finalised tier missing height 5 (a chain-view hole) at or below the
        // tip. A range whose bracket covers height 5 must fail loud rather than
        // silently drop a block that might be in range.
        let times = linear_times(13);
        let fs = (0..=12)
            .filter(|h| *h != 5)
            .map(|h| {
                block_at(
                    h,
                    u8::try_from(0x10 + h).expect("tag fits u8"),
                    times[usize::try_from(h).expect("height fits usize")],
                )
            })
            .collect();
        let engine = engine_over_tiers(fs, Vec::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");

        match BlockHashRead::block_hashes(&snapshot, times[3], times[8] + 1).await {
            Err(BlockHashReadError::MissingHeader { height }) => {
                assert_eq!(u32::from(height), 5);
            }
            other => panic!("a chain-view hole must be a typed MissingHeader, got {other:?}"),
        }
    }
}

mod chain_info_reads {
    use super::*;
    use zaino_service::ChainInfoRead;
    use zaino_service::error::ReadError;
    use zaino_source::FailureMode;
    use zaino_source::mock::sample_blockchain_info;

    /// A single-attempt policy: one injected failure is terminal, so an error
    /// test cannot be masked by the default policy's retries.
    fn single_attempt() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// An engine with an empty local view over `source`, single-attempt, so an
    /// injected transport failure is terminal.
    fn engine_single_attempt(source: MockChain) -> LightEngine {
        Engine::new(
            StubNonFinalised::empty(),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, single_attempt()),
        )
    }

    #[tokio::test]
    async fn chain_info_passes_the_validators_whole_blockchain_info_through() {
        let engine = engine_with(MockChain::new().with_blockchain_info(sample_blockchain_info()));
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let info = ChainInfoRead::chain_info(&snapshot)
            .await
            .expect("chain info served");
        // Assert each field a consumer reads individually, against the fixture's
        // distinguishable values, so dropping or defaulting any one of them fails
        // this test rather than silently blanking an explorer view.
        let expected = sample_blockchain_info();
        assert_eq!(info.blocks, expected.blocks);
        assert_eq!(info.difficulty.to_bits(), expected.difficulty.to_bits());
        assert_eq!(info.chain, expected.chain);
        assert_eq!(info.value_pools, expected.value_pools);
        assert_eq!(info.size_on_disk, expected.size_on_disk);
        assert_eq!(info.commitments, expected.commitments);
    }

    #[tokio::test]
    async fn an_unreachable_validator_errors_rather_than_defaulting() {
        // The explorer polls getblockchaininfo every 15s; a defaulted success
        // would blank four of its views, while an error leaves the previous
        // values in place. A scripted response is present, but the single-attempt
        // transport failure is terminal: the read must surface it, not answer a
        // zeroed BlockchainInfo.
        let engine = engine_single_attempt(
            MockChain::new()
                .with_blockchain_info(sample_blockchain_info())
                .fail_next(1, FailureMode::Connection),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match ChainInfoRead::chain_info(&snapshot).await {
            Err(ReadError::Transient(_)) => {}
            other => panic!("an unreachable validator must error, not default: {other:?}"),
        }
    }
}

// --- resolved-transaction reads: always passthrough -------------------------
//
// `TransactionViewRead` relays the validator's decoded transaction / decoded
// block and resolves every transparent input to the output it spends. A miss on
// the requested transaction or block is `Ok(None)`.
mod transaction_view_reads {
    use super::*;
    use zaino_primitives::types::{
        BlockHash, BlockSelector, CoinbaseInput, Script, Transaction, TransactionDetail,
        TransactionLocation, TransparentData, TransparentInput, TransparentOutput, Zatoshis,
    };
    use zaino_service::TransactionViewRead;
    use zaino_service::error::TransactionViewError;
    use zaino_source::mock::sample_decoded_block;

    fn id(byte: u8) -> TransactionId {
        TransactionId::from([byte; 32])
    }

    /// A v5 non-coinbase envelope — the mock's default for a decoded transaction.
    fn plain_detail() -> TransactionDetail {
        TransactionDetail {
            version: 5,
            overwintered: true,
            version_group_id: Some(0x26A7_270A),
            lock_time: 0,
            expiry_height: Some(height(0)),
            size: 180,
            coinbase: None,
            transparent_inputs: Vec::new(),
            joinsplits: Vec::new(),
        }
    }

    /// A coinbase envelope: the input lives here, not in the indexing shape.
    fn coinbase_detail() -> TransactionDetail {
        TransactionDetail {
            coinbase: Some(CoinbaseInput {
                script: Script::new(vec![0x03, 0x01, 0x02]),
                sequence: 0xffff_ffff,
            }),
            ..plain_detail()
        }
    }

    /// A transaction whose one transparent input spends an *external* txid and
    /// which carries one output, so the mock (which answers any txid with this
    /// same transaction) resolves the prevout to this output.
    fn spending_tx() -> Transaction {
        Transaction {
            txid: id(7),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: id(9),
                    prev_index: 0,
                }],
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(4200).expect("amount in range"),
                    script: Script::new(vec![0x51]),
                }],
            },
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    fn coinbase_tx() -> Transaction {
        Transaction {
            txid: id(1),
            transparent: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    #[tokio::test]
    async fn transaction_view_of_a_coinbase_has_no_inputs_to_resolve() {
        let engine = engine_with(
            MockChain::new()
                .respond_transaction_verbose(
                    coinbase_tx(),
                    TransactionLocation::BestChain(height(9)),
                )
                .with_detail(coinbase_detail()),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let located = TransactionViewRead::transaction_view(&snapshot, id(1))
            .await
            .expect("served")
            .expect("present");
        // Coinbase-ness is data in the detail; the indexing shape has no inputs, so
        // nothing is resolved and no prevout is fetched.
        assert!(located.view.inputs.is_empty());
        assert!(located.view.detail.coinbase.is_some());
        assert_eq!(located.location, TransactionLocation::BestChain(height(9)));
    }

    #[tokio::test]
    async fn transaction_view_resolves_a_prevout_the_validator_knows() {
        let spender = spending_tx();
        let engine = engine_with(
            MockChain::new()
                .respond_transaction_verbose(spender.clone(), TransactionLocation::Mempool),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let located = TransactionViewRead::transaction_view(&snapshot, id(7))
            .await
            .expect("served")
            .expect("present");
        assert_eq!(located.view.inputs.len(), 1);
        // The input names the external outpoint it spends, resolved to the spent
        // output — here the validator's canned transaction's own output.
        assert_eq!(
            located.view.inputs[0].outpoint,
            spender.transparent.inputs[0]
        );
        assert_eq!(located.view.inputs[0].spent, spender.transparent.outputs[0]);
    }

    #[tokio::test]
    async fn an_unknown_txid_is_a_served_none() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let answer = TransactionViewRead::transaction_view(&snapshot, id(3))
            .await
            .expect("a domain miss is a served None, not an error");
        assert!(answer.is_none());
    }

    /// R52: the validator serves the spending transaction but does not know the
    /// txid its one input spends, so the prevout cannot be resolved. The read
    /// surfaces a named `MissingPrevout` — the exact outpoint, not a blank value —
    /// rather than inventing a zero input. The whitelist serves only the spender's
    /// own txid; the prevout misses. No failure is injected, so the terminal domain
    /// not-found is not something retries could mask, and `engine_with`'s default
    /// policy is correct here.
    #[tokio::test]
    async fn transaction_view_of_an_unknown_prevout_is_missing_prevout() {
        let spender = spending_tx();
        let engine = engine_with(
            MockChain::new()
                .respond_transaction_verbose(spender.clone(), TransactionLocation::Mempool)
                .restrict_transaction_verbose_to(spender.txid),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match TransactionViewRead::transaction_view(&snapshot, id(7)).await {
            Err(TransactionViewError::MissingPrevout { outpoint }) => {
                assert_eq!(outpoint, spender.transparent.inputs[0]);
            }
            other => panic!("an unknown prevout must be a named MissingPrevout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_transaction_views_by_height_reads_the_by_height_port() {
        // The two ports hold blocks with distinct tags, so a swapped selector
        // returns the wrong block and this test fails.
        let engine = engine_with(
            MockChain::new()
                .with_block_decoded(sample_decoded_block(0x10, 500))
                .with_block_decoded_by_hash(sample_decoded_block(0x20, 900)),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let views = TransactionViewRead::block_transaction_views(
            &snapshot,
            BlockSelector::Height(height(5)),
        )
        .await
        .expect("served")
        .expect("present");
        assert_eq!(views.size, 500);
        // The coinbase is first; its txid carries the by-height tag, not by-hash's.
        assert_eq!(views.transactions[0].transaction.txid, id(0x10));
        assert!(views.transactions[0].detail.coinbase.is_some());
    }

    #[tokio::test]
    async fn block_transaction_views_by_hash_reads_the_by_hash_port() {
        let engine = engine_with(
            MockChain::new()
                .with_block_decoded(sample_decoded_block(0x10, 500))
                .with_block_decoded_by_hash(sample_decoded_block(0x20, 900)),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let views = TransactionViewRead::block_transaction_views(
            &snapshot,
            BlockSelector::Hash(BlockHash::from([2u8; 32])),
        )
        .await
        .expect("served")
        .expect("present");
        assert_eq!(views.size, 900);
        assert_eq!(views.transactions[0].transaction.txid, id(0x20));
    }

    #[tokio::test]
    async fn an_unknown_block_is_a_served_none() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        assert!(
            TransactionViewRead::block_transaction_views(
                &snapshot,
                BlockSelector::Height(height(99))
            )
            .await
            .expect("a domain miss is a served None, not an error")
            .is_none()
        );
        assert!(
            TransactionViewRead::block_transaction_views(
                &snapshot,
                BlockSelector::Hash(BlockHash::from([9u8; 32]))
            )
            .await
            .expect("a domain miss is a served None, not an error")
            .is_none()
        );
    }

    #[tokio::test]
    async fn decoded_block_by_height_reads_the_by_height_port() {
        // Distinct blocks per port, so a swapped selector returns the wrong one.
        let engine = engine_with(
            MockChain::new()
                .with_block_decoded(sample_decoded_block(0x10, 500))
                .with_block_decoded_by_hash(sample_decoded_block(0x20, 900)),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let decoded =
            TransactionViewRead::decoded_block(&snapshot, BlockSelector::Height(height(5)))
                .await
                .expect("served")
                .expect("present");
        assert_eq!(decoded.size, 500);
        assert_eq!(decoded.transactions[0].transaction.txid, id(0x10));
        assert!(decoded.transactions[0].detail.coinbase.is_some());
    }

    #[tokio::test]
    async fn decoded_block_by_hash_reads_the_by_hash_port() {
        let engine = engine_with(
            MockChain::new()
                .with_block_decoded(sample_decoded_block(0x10, 500))
                .with_block_decoded_by_hash(sample_decoded_block(0x20, 900)),
        );
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        let decoded = TransactionViewRead::decoded_block(
            &snapshot,
            BlockSelector::Hash(BlockHash::from([2u8; 32])),
        )
        .await
        .expect("served")
        .expect("present");
        assert_eq!(decoded.size, 900);
        assert_eq!(decoded.transactions[0].transaction.txid, id(0x20));
    }

    #[tokio::test]
    async fn decoded_block_of_an_unknown_block_is_a_served_none() {
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        assert!(
            TransactionViewRead::decoded_block(&snapshot, BlockSelector::Height(height(99)))
                .await
                .expect("a domain miss is a served None, not an error")
                .is_none()
        );
        assert!(
            TransactionViewRead::decoded_block(
                &snapshot,
                BlockSelector::Hash(BlockHash::from([9u8; 32]))
            )
            .await
            .expect("a domain miss is a served None, not an error")
            .is_none()
        );
    }
}
