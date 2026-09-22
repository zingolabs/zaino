//! Composed-engine tests: the profile-coverage assertion, the light-serve
//! acceptance gate, and the per-capability passthrough routing tests.
//!
//! Exercised entirely with in-crate mocks — two empty stub views for the
//! composed chain and a [`MockChain`] for the validator source, wrapped in the
//! [`ValidatorClient`] decorator exactly as the root injects it. No cluster, no
//! validator; the routing is deterministic.

use zaino_chainview::testing::StubNonFinalised;
use zaino_service::error::{AddressReadError, BroadcastRejection, TreestateReadError};
use zaino_service::{AddressRead, Broadcast, RawTransactionRead, TakeSnapshot, TreestateRead};
use zaino_source::mock::MockChain;
use zaino_source::{RetryPolicy, SendRawTransactionError, ValidatorClient};

use zaino_core::{
    Height, HeightRange, RawTransaction, ShieldedPool, TransactionId, TransactionLocation,
    TransparentAddress,
};

use crate::Engine;

/// The milestone: the composed engine type-checks as every public profile, over
/// any two composer inputs (each a `TakeSnapshot` whose snapshot is a
/// `ChainSegment + CompactBlockRead`). Compile-time only.
fn _engine_satisfies_all_profiles<Fs, Nfs, Src>()
where
    Fs: TakeSnapshot<Snapshot: zaino_service::ChainSegment + zaino_service::CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: zaino_service::ChainSegment + zaino_service::CompactBlockRead>,
    Src: zaino_source::GetTreestate
        + zaino_source::SendRawTransaction
        + zaino_source::GetAddressBalance
        + zaino_source::GetAddressUtxos
        + zaino_source::GetAddressTxids
        + zaino_source::GetAddressDeltas
        + zaino_source::GetTransaction
        + zaino_source::GetSubtreeRoots
        + Clone
        + 'static,
    Engine<Fs, Nfs, Src>: zaino_service::WalletLibService
        + zaino_service::LightServeService
        + zaino_service::NodeRpcService,
{
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("valid height")
}

// The engine consumes the *canonical* (resilient) source, so the mock is wrapped
// in the ValidatorClient decorator — exactly how the root injects it.
fn engine_with(
    source: MockChain,
) -> Engine<StubNonFinalised, StubNonFinalised, ValidatorClient<MockChain>> {
    Engine::new(
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

// The acceptance gate for the full light-wallet read-set (#10): the composed
// engine must serve every read `LightServeService` demands — none reporting
// itself `NotServiceable`. Red today (per-block reads / nullifiers / subtree
// roots are still stubs); un-ignore each cap's grind as it greens, and drop the
// `#[ignore]` when the whole read-set is wired. The per-cap tests below are the
// incremental, always-run signal on the way there.
#[tokio::test]
#[ignore = "acceptance: un-ignore when the full light-wallet read-set is served (#10)"]
async fn light_serve_conformance_over_a_provisioned_source() {
    let engine = engine_with(MockChain::new());
    zaino_service::conformance::assert_light_serve_conformance(&engine).await;
}

#[tokio::test]
async fn address_reads_pass_through_and_answer() {
    // No rejection seeded: the mock answers empty (no-match) results. An `Ok` —
    // not a `NotServiceable` stub — proves each address read routes to the
    // passthrough provider.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("t1ExampleProbeAddress0000000000000000".to_string());
    let range = HeightRange {
        start: height(0),
        end: height(10),
    };
    AddressRead::balance(&snapshot, &addr, range)
        .await
        .expect("balance served");
    assert!(AddressRead::unspent_outpoints(&snapshot, &addr)
        .await
        .expect("utxos served")
        .is_empty());
    assert!(AddressRead::tx_ids(&snapshot, &addr, range)
        .await
        .expect("txids served")
        .is_empty());
    assert!(AddressRead::deltas(&snapshot, &addr, range)
        .await
        .expect("deltas served")
        .is_empty());
}

#[tokio::test]
async fn address_reads_map_an_invalid_address_to_fatal() {
    // The mock rejects the addresses as invalid; the remote provider maps that
    // domain rejection to a definitive (non-retryable) read failure, proving the
    // read routes to the source and its rejection is surfaced.
    let engine = engine_with(MockChain::new().reject_addresses("bad t-addr"));
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let addr = TransparentAddress::new("bogus".to_string());
    let range = HeightRange {
        start: height(0),
        end: height(10),
    };
    match AddressRead::balance(&snapshot, &addr, range).await {
        Err(AddressReadError::Fatal(msg)) => {
            assert!(msg.contains("invalid address"), "got: {msg}")
        }
        other => panic!("expected a definitive invalid-address failure, got {other:?}"),
    }
}

#[tokio::test]
async fn raw_transaction_passes_through_the_bytes_and_location() {
    // Seed a canned response: passthrough must relay the exact bytes and where
    // the validator placed the transaction, unparsed.
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
    // No response seeded: the mock answers NotFound, a domain miss the passthrough
    // maps to `Ok(None)` — not a NotServiceable stub.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let got = RawTransactionRead::raw_transaction(&snapshot, TransactionId::from([2u8; 32]))
        .await
        .expect("served");
    assert_eq!(got, None);
}

#[tokio::test]
async fn subtree_roots_pass_through_from_an_index() {
    // An `Ok` — not a NotServiceable stub — proves the index-addressed subtree
    // read routes to the passthrough provider.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    let roots = TreestateRead::subtree_roots(&snapshot, ShieldedPool::Sapling, 0, None)
        .await
        .expect("subtree roots served");
    assert!(roots.is_empty());
}

#[tokio::test]
async fn treestate_passes_through_a_missing_height() {
    // No treestate seeded: the mock answers HeightNotFound, which the remote
    // provider maps to a definitive read failure. Proves treestate routes to the
    // passthrough provider (the read-set's per-cap classification) — not a
    // NotServiceable stub.
    let engine = engine_with(MockChain::new());
    let snapshot = engine.snapshot().await.expect("snapshot acquired");
    match TreestateRead::treestate(&snapshot, height(5)).await {
        Err(TreestateReadError::Fatal(msg)) => {
            assert!(msg.contains("no treestate at height"), "got: {msg}")
        }
        other => panic!("expected a definitive miss, got {other:?}"),
    }
}
