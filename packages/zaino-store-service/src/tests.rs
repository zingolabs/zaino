//! Composed-engine tests: the light-serve acceptance gate, the per-capability
//! routing tests under `LightRouting`, the manifest derivation, and the seam
//! split a local merge relies on.
//!
//! Exercised entirely with in-crate mocks — stub views for the composed chain
//! and a [`MockChain`] for the validator source, wrapped in the
//! [`ValidatorClient`] decorator exactly as the root injects it. No cluster, no
//! validator; the routing is deterministic.

use futures::stream::StreamExt;
use zaino_chainview::testing::{stub_compact_block, StubNonFinalised};
use zaino_service::error::{AddressReadError, BroadcastRejection, TreestateReadError};
use zaino_service::routing::LightRouting;
use zaino_service::testing::{MockChain as MockService, MockIndexerService};
use zaino_service::{
    AddressRead, Broadcast, MempoolContent, MempoolSubscribe, RawTransactionRead, Serviceable,
    TakeSnapshot, TreestateRead,
};
use zaino_source::mock::MockChain;
use zaino_source::{RetryPolicy, SendRawTransactionError, ValidatorClient};

use zaino_core::{
    Answerable, BlockId, Capability, Height, HeightRange, RawTransaction, ShieldedPool,
    TransactionId, TransactionLocation, TransparentAddress,
};

use crate::composed::split_at_seam;
use crate::Composed;

type LightEngine =
    Composed<StubNonFinalised, StubNonFinalised, ValidatorClient<MockChain>, LightRouting>;

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
    Composed::new(
        StubNonFinalised::empty(),
        StubNonFinalised::empty(),
        ValidatorClient::new(source, RetryPolicy::default()),
    )
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

// The acceptance gate for the full light-wallet read-set: under `LightRouting`
// the composed engine serves every read `LightServeService` demands — none
// reporting itself `NotServiceable`. The per-cap tests below pin each
// capability's placement.
#[tokio::test]
async fn light_serve_conformance_over_a_provisioned_source() {
    let engine = engine_with(MockChain::new());
    zaino_service::conformance::assert_light_serve_conformance(&engine).await;
}

#[tokio::test]
async fn address_reads_are_remote_under_light_routing() {
    // No rejection seeded: the mock answers empty (no-match) results. An `Ok` —
    // not a `NotServiceable` stub — proves each address read routes to the
    // passthrough provider, as `LightRouting::Address = Remote` says.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("t1ExampleProbeAddress0000000000000000".to_string());
    AddressRead::balance(&snapshot, &addr, range(0, 10))
        .await
        .expect("balance served");
    assert!(AddressRead::unspent_outpoints(&snapshot, &addr)
        .await
        .expect("utxos served")
        .is_empty());
    assert!(AddressRead::tx_ids(&snapshot, &addr, range(0, 10))
        .await
        .expect("txids served")
        .is_empty());
    assert!(AddressRead::deltas(&snapshot, &addr, range(0, 10))
        .await
        .expect("deltas served")
        .is_empty());
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
    use zaino_core::BlockRef;
    use zaino_service::CompactNullifierRead;
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let got =
        CompactNullifierRead::compact_block_nullifiers(&snapshot, BlockRef::Height(height(0)))
            .await
            .expect("served");
    assert!(got.is_none());
}

#[tokio::test]
async fn treestate_is_remote_under_light_routing() {
    // No treestate seeded: the mock answers HeightNotFound, which the remote
    // provider maps to a definitive read failure. Proves treestate routes to the
    // passthrough provider, as `LightRouting::Treestate = Remote` says.
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
) -> Composed<MockIndexerService, StubNonFinalised, ValidatorClient<MockChain>, LightRouting> {
    let fs = MockIndexerService::new(MockService {
        tip: tip.map(|h| BlockId {
            height: height(h),
            hash: [0u8; 32].into(),
        }),
        ..MockService::default()
    });
    Composed::new(
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
    // Remote: live, the validator answers.
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
    // Remote and withheld are unaffected by the store's progress.
    assert_eq!(manifest.get(Capability::Treestate), Answerable::Live);
    assert_eq!(manifest.get(Capability::SpendStatus), Answerable::Absent);
}

// --- the seam split a local merge relies on ----------------------------------

/// A finalised side covering `[0, tip]` and an empty head.
async fn pinned_with_finalised_tip(
    tip: u32,
) -> zaino_chainview::ChainViewSnapshot<StubNonFinalised, StubNonFinalised> {
    let blocks = (0..=tip)
        .map(|h| stub_compact_block(h, 1))
        .collect::<Vec<_>>();
    zaino_chainview::ChainView::new(
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
    // Straddling: `[5, 11)` is the store's, `[11, 20)` the head's.
    assert_eq!(
        split_at_seam(&local, range(5, 20)),
        (Some(range(5, 11)), Some(range(11, 20)))
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
    // Ending exactly at the seam: the watermark height itself is the store's.
    assert_eq!(
        split_at_seam(&local, range(8, 11)),
        (Some(range(8, 11)), None)
    );
    // Empty.
    assert_eq!(split_at_seam(&local, range(7, 7)), (None, None));
}

#[tokio::test]
async fn with_no_watermark_the_whole_range_is_the_heads() {
    let local =
        zaino_chainview::ChainView::new(StubNonFinalised::empty(), StubNonFinalised::empty())
            .snapshot()
            .await
            .expect("snapshot");
    assert_eq!(
        split_at_seam(&local, range(3, 9)),
        (None, Some(range(3, 9)))
    );
}
