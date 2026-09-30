//! Scenarios against fake endpoints. No real node, no `zaino-source` mock (it holds a chain,
//! not a mempool, and cannot relay).

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use zaino_primitives::types::PeerInfo;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth, TransactionId, Zatoshis};
use zaino_source::{
    BlockLink, FailureMode, GetBlockLink, GetBlockLinkError, GetChainTip, GetChainTipError,
    GetMempoolListing, GetMempoolListingError, GetMempoolSourceTip, GetPeerInfo, GetPeerInfoError,
    GetRawMempoolTransaction, GetRawMempoolTransactionError, MempoolListed, NonDomainError,
    QueryError, SendRawTransaction, SendRawTransactionError, SourceTip,
};

use crate::endpoint::Polled;
use crate::endpoints::EndpointIndex;
use crate::{
    Agreement, BelowQuorum, BroadcastError, ChainView, Endpoint, EndpointSet, EndpointState,
    MempoolEntry,
};

/// What one validator would answer, mutated between ticks by the test.
///
/// - Best chain = `tip`, then `branch` overrides, then the shared trunk (`trunk(h)`)
#[derive(Default)]
struct FakeState {
    tip: Option<BlockRef>,
    branch: BTreeMap<Height, BlockHash>,
    not_ready: bool,
    mempool_inactive: bool,
    /// `None` = at the tip it reports
    network_tip: Option<Height>,
    listed: BTreeSet<TransactionId>,
    bytes: BTreeMap<TransactionId, Vec<u8>>,
    peers: Vec<String>,
    /// Next header answers come from this chain instead (the node reorged after its tip read)
    links_from: Option<BTreeMap<Height, BlockHash>>,
    links_served: usize,
    /// `None` accepts and echoes the txid back; `Some` is this node's answer to a relay.
    relay: Option<Result<(), SendRawTransactionError>>,
    relay_unreachable: bool,
}

/// Every fake's default ancestry: one chain all of them share below their own blocks
fn trunk(height: Height) -> BlockHash {
    let mut hash = [0xee; 32];
    hash[..4].copy_from_slice(&u32::from(height).to_le_bytes());
    BlockHash::from(hash)
}

impl FakeState {
    fn hash_at(&self, height: Height) -> BlockHash {
        match self.tip {
            Some(tip) if tip.height == height => tip.hash,
            _ => self.branch.get(&height).copied().unwrap_or_else(|| trunk(height)),
        }
    }
}

#[derive(Default)]
struct FakeValidator(Mutex<FakeState>);

impl FakeValidator {
    fn edit(&self, edit: impl FnOnce(&mut FakeState)) {
        edit(&mut self.0.lock().expect("fake validator mutex poisoned"))
    }
}

impl GetBlockLink for FakeValidator {
    async fn get_block_link(
        &self,
        height: Height,
    ) -> Result<BlockLink, QueryError<GetBlockLinkError>> {
        let mut fake = self.0.lock().expect("fake validator mutex poisoned");
        fake.links_served += 1;
        let tip = fake.tip.map_or(Height::GENESIS, |tip| tip.height);
        if height > tip {
            return Err(QueryError::Domain(GetBlockLinkError::HeightNotFound(height)));
        }
        let at = |h| match &fake.links_from {
            Some(moved) => moved.get(&h).copied().unwrap_or_else(|| trunk(h)),
            None => fake.hash_at(h),
        };
        let prev_hash = height.checked_sub(1).map_or(BlockHash::ZERO, at);
        Ok(BlockLink { hash: at(height), prev_hash })
    }
}

impl GetChainTip for FakeValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        if fake.not_ready {
            return Err(QueryError::Domain(GetChainTipError::NotReady));
        }
        let tip = fake.tip.unwrap_or(BlockRef { hash: BlockHash::ZERO, height: Height::GENESIS });
        Ok((tip.hash, tip.height))
    }
}

impl GetMempoolSourceTip for FakeValidator {
    async fn get_mempool_source_tip(&self) -> Result<SourceTip, QueryError<Infallible>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        let tip = fake.tip.unwrap_or(BlockRef { hash: BlockHash::ZERO, height: Height::GENESIS });
        let estimated_height = fake.network_tip.unwrap_or(tip.height);
        Ok(SourceTip { hash: tip.hash, height: tip.height, estimated_height })
    }
}

/// Fee a fake lists for `txid` (a function of the tx, as on a real validator)
fn fee_of(txid: &TransactionId) -> Zatoshis {
    Zatoshis::new(u64::from(<[u8; 32]>::from(*txid)[0]) * 1_000).expect("in supply")
}

impl GetMempoolListing for FakeValidator {
    async fn get_mempool_listing(
        &self,
    ) -> Result<Vec<MempoolListed>, QueryError<GetMempoolListingError>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        if fake.mempool_inactive {
            return Err(QueryError::Domain(GetMempoolListingError::Inactive));
        }
        Ok(fake
            .listed
            .iter()
            .map(|txid| MempoolListed { txid: *txid, fee: fee_of(txid) })
            .collect())
    }
}

impl GetRawMempoolTransaction for FakeValidator {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        fake.bytes
            .get(&txid)
            .cloned()
            .ok_or(QueryError::Domain(GetRawMempoolTransactionError::NotFound(txid)))
    }
}

impl GetPeerInfo for FakeValidator {
    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        Ok(fake.peers.iter().map(|addr| PeerInfo { addr: addr.clone(), inbound: false }).collect())
    }
}

/// Test windows: 3 ancestors below each tip
fn depth() -> ReorgDepth {
    ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"))
}

impl SendRawTransaction for FakeValidator {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        let fake = self.0.lock().expect("fake validator mutex poisoned");
        if fake.relay_unreachable {
            return Err(QueryError::NonDomain(NonDomainError::new(
                FailureMode::Connection,
                "fake validator unreachable",
            )));
        }
        match &fake.relay {
            Some(Err(rejected)) => Err(QueryError::Domain(rejected.clone())),
            _ => Ok(TransactionId::from([transaction[0]; 32])),
        }
    }
}

/// N=1: quorum trivially met. A tail = its anchor's snapshot, then each later crossing once
/// (never one the snapshot carried), silent on an empty mempool, ended by a mined block.
#[tokio::test]
async fn a_single_endpoint_tail_sends_its_snapshot_then_each_arrival_once_until_a_block() {
    let validator = Arc::new(FakeValidator::default());
    validator.edit(|fake| {
        fake.tip = Some(BlockRef {
            hash: BlockHash::from([10u8; 32]),
            height: Height::try_from(10).expect("10 is in range"),
        });
        fake.listed =
            [TransactionId::from([1u8; 32]), TransactionId::from([2u8; 32])].into_iter().collect();
        fake.bytes = [
            (TransactionId::from([1u8; 32]), vec![1u8; 8]),
            (TransactionId::from([2u8; 32]), vec![2u8; 8]),
        ]
        .into_iter()
        .collect();
    });

    let (view, pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    let reader = view.subscriber();

    assert_eq!(reader.quorum().threshold(), 1, "⌊1/2⌋ + 1");
    assert!(reader.current().mempool().is_err(), "nothing polled: no agreed tip, no answer");
    assert!(reader.tail().is_err(), "below quorum: the stream is refused, not opened silent");

    assert_eq!(pollers[0].tick().await.expect("first poll succeeds"), Polled::Listed(2));
    let pinned = reader.current();
    let tip_10 =
        BlockRef { hash: BlockHash::from([10u8; 32]), height: Height::try_from(10).expect("10") };
    assert_eq!(pinned.tip().expect("one endpoint = a majority of one").block, tip_10);
    let entry = |seed: u8, fee: u64| MempoolEntry {
        txid: TransactionId::from([seed; 32]),
        raw: Bytes::from(vec![seed; 8]),
        fee: Some(Zatoshis::new(fee).expect("in supply")),
    };
    let entries: Vec<_> = pinned.mempool().expect("quorum met").entries().collect();
    assert_eq!(entries, [entry(1, 1_000), entry(2, 2_000)], "each entry: its validator's fee");

    let mut tail = reader.tail().expect("quorum met");
    let snapshot: Vec<_> = tail.snapshot().entries().collect();
    assert_eq!(snapshot, [entry(1, 1_000), entry(2, 2_000)], "the whole servable mempool");

    // tx 2 flaps out and back: it rode the snapshot, so its re-crossing is not re-sent
    // tx 1 is dropped (propagation churn): nothing to send, no entry
    // tx 3 arrives, and ours (tx 9) is servable before any listing
    validator.edit(|fake| fake.listed.retain(|txid| *txid != TransactionId::from([2u8; 32])));
    pollers[0].tick().await.expect("second poll succeeds");
    validator.edit(|fake| {
        fake.listed = [2u8, 3].map(|seed| TransactionId::from([seed; 32])).into_iter().collect();
        fake.bytes.insert(TransactionId::from([3u8; 32]), vec![3u8; 8]);
    });
    pollers[0].tick().await.expect("third poll succeeds");
    let ours = view.broadcast(vec![9u8; 8]).await.expect("accepted");
    let arrivals: Vec<_> = reader.current().arrivals().iter().copied().collect();
    let crossings = [1u8, 2, 2, 3].map(|seed| TransactionId::from([seed; 32]));
    let expected = [&crossings[..], &[ours]].concat();
    assert_eq!(arrivals, expected, "every crossing this epoch, in order, a re-crossing again");

    assert_eq!(tail.next().await, Some(entry(3, 3_000)), "tx 2 was in the snapshot: skipped");
    let unpriced = MempoolEntry { txid: ours, raw: Bytes::from(vec![9u8; 8]), fee: None };
    assert_eq!(tail.next().await, Some(unpriced), "our own send, before any validator lists it");
    let silent = tokio::time::timeout(Duration::from_millis(50), tail.next()).await;
    assert!(silent.is_err(), "nothing new, no block mined: a live, silent stream");

    // A late subscriber's snapshot already holds all three: its tail starts past them
    let mut late = reader.tail().expect("quorum met");
    let late_snapshot: Vec<_> = late.snapshot().entries().map(|entry| entry.txid).collect();
    assert_eq!(late_snapshot, [crossings[1], crossings[3], ours]);

    // Block 11: the one thing that ends a stream, for every tail on the old tip
    validator.edit(|fake| {
        fake.tip = Some(BlockRef {
            hash: BlockHash::from([11u8; 32]),
            height: Height::try_from(11).expect("11 is in range"),
        })
    });
    pollers[0].tick().await.expect("fourth poll succeeds");
    assert_eq!(tail.next().await, None, "the stream ends on a mined block");
    assert_eq!(late.next().await, None, "late subscriber too");
    assert_eq!(tail.next().await, None, "and stays ended");
    assert!(reader.current().arrivals().is_empty(), "the new tip starts a fresh log");
}

/// A validator whose mempool is off below the network tip still votes its tip (the sync
/// producer's only input) and serves an empty mempool, then lists again once active.
#[tokio::test]
async fn a_catching_up_validator_votes_its_tip_with_no_mempool() {
    let validator = Arc::new(FakeValidator::default());
    let tip_at = |height: u8| BlockRef {
        hash: BlockHash::from([height; 32]),
        height: Height::try_from(u32::from(height)).expect("small heights are in range"),
    };
    validator.edit(|fake| {
        fake.tip = Some(tip_at(10));
        fake.listed = [TransactionId::from([1u8; 32])].into_iter().collect();
        fake.bytes = [(TransactionId::from([1u8; 32]), vec![1u8; 8])].into_iter().collect();
    });

    let (view, pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    let reader = view.subscriber();
    let tip = reader.subscribe_tip();

    assert_eq!(pollers[0].tick().await.expect("active poll succeeds"), Polled::Listed(1));

    validator.edit(|fake| {
        fake.tip = Some(tip_at(11));
        fake.network_tip = Some(tip_at(40).height);
        fake.mempool_inactive = true;
    });
    assert_eq!(
        pollers[0].tick().await.expect("an inactive mempool is an answer, not a failure"),
        Polled::CatchingUp { tip: tip_at(11), network: tip_at(40).height },
    );
    assert_eq!(tip.borrow().map(|quorum| quorum.block), Some(tip_at(11)), "tip still voted");
    let catching_up = reader.current();
    assert_eq!(catching_up.endpoints()[0].state, EndpointState::CatchingUp);
    assert_eq!(catching_up.endpoints()[0].failures, 0);
    assert_eq!(
        catching_up.mempool().expect("quorum met on the voted tip").entries().count(),
        0,
        "sighting retracted with the mempool off",
    );

    validator.edit(|fake| {
        fake.tip = Some(tip_at(12));
        fake.network_tip = None;
        fake.mempool_inactive = false;
        fake.listed = [TransactionId::from([2u8; 32])].into_iter().collect();
        fake.bytes = [(TransactionId::from([2u8; 32]), vec![2u8; 8])].into_iter().collect();
    });
    assert_eq!(pollers[0].tick().await.expect("caught-up poll succeeds"), Polled::Listed(1));
    let caught_up = reader.current();
    assert_eq!(caught_up.endpoints()[0].state, EndpointState::Live);
    assert_eq!(
        caught_up.mempool().expect("quorum met").entries().map(|e| e.txid).collect::<Vec<_>>(),
        [TransactionId::from([2u8; 32])],
    );
}

/// Quorum is over the configured set: one of three answering is not a majority, two agreeing is,
/// and a third claiming a lone higher tip moves nothing.
#[tokio::test]
async fn a_lone_higher_tip_does_not_move_a_quorum_of_three() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let agreed = BlockRef {
        hash: BlockHash::from([100u8; 32]),
        height: Height::try_from(100).expect("100 is in range"),
    };
    let tx7 = TransactionId::from([7u8; 32]);
    for validator in &validators {
        validator.edit(|fake| {
            fake.tip = Some(agreed);
            fake.bytes = [(tx7, vec![7u8; 8])].into_iter().collect();
            fake.peers = vec!["seed-a:8233".to_string()];
        });
    }
    validators[0].edit(|fake| fake.listed = [tx7].into_iter().collect());

    let (view, pollers) = ChainView::new(
        validators
            .iter()
            .zip(["a:8232", "b:8232", "c:8232"])
            .map(|(source, address)| Endpoint {
                address: address.to_owned(),
                source: Arc::clone(source),
            })
            .collect(),
        depth(),
    )
    .expect("three endpoints is a valid set");
    let reader = view.subscriber();

    assert_eq!(reader.quorum().threshold(), 2, "⌊3/2⌋ + 1");

    // One of three = below threshold: no tip, no mempool answer (endpoint healthy, reporting)
    pollers[0].tick().await.expect("endpoint a polls");
    let pinned = reader.current();
    assert_eq!(pinned.tip(), None, "one of three configured is not a quorum");
    assert!(pinned.mempool().is_err(), "fail closed below threshold");
    let sighting = pinned.sighting(&tx7).expect("endpoint a reported it");
    assert_eq!(sighting.seen_at().count(), 1, "sighting recorded, just not servable");

    // Two agreeing by hash = quorum met; the tx only endpoint a lists is still not servable
    pollers[1].tick().await.expect("endpoint b polls");
    let pinned = reader.current();
    let tip = pinned.tip().expect("two of three agree");
    assert_eq!(tip.block, agreed);
    let a_and_b = [0, 1].map(|index| EndpointIndex::new(index).expect("index is in range"));
    assert_eq!(tip.agreed_by, a_and_b.into_iter().collect());
    let mempool = pinned.mempool().expect("quorum met");
    assert!(mempool.get(&tx7).is_none(), "1 of 3 sightings < the per-transaction threshold");

    // Endpoint c alone claims a far higher tip — agrees with nobody, so it moves nothing
    validators[2].edit(|fake| {
        fake.tip = Some(BlockRef {
            hash: BlockHash::from([250u8; 32]),
            height: Height::try_from(999_999).expect("999999 is in range"),
        });
        fake.listed = [tx7].into_iter().collect();
        fake.peers = vec!["seed-z:8233".to_string()];
    });
    pollers[2].tick().await.expect("endpoint c polls");
    let pinned = reader.current();
    let tip = pinned.tip().expect("a and b still agree");
    assert_eq!(tip.block, agreed, "quorum tip = highest *agreed* block, never highest claimed");
    let mempool = pinned.mempool().expect("quorum met");
    assert!(mempool.get(&tx7).is_some(), "a and c both report it = the threshold");
    let peers: Vec<(&str, Vec<String>)> = pinned
        .endpoints()
        .iter()
        .map(|meta| (meta.address.as_str(), meta.peers.iter().cloned().collect()))
        .collect();
    let expected =
        [("a:8232", "seed-a:8233"), ("b:8232", "seed-a:8233"), ("c:8232", "seed-z:8233")]
            .map(|(address, peer)| (address, vec![peer.to_owned()]));
    assert_eq!(peers, expected, "each validator's peers, keyed by its configured address");

    let arrivals: Vec<TransactionId> = pinned.arrivals().iter().copied().collect();
    assert_eq!(arrivals, [tx7], "threshold crossing logged exactly once, at the crossing");
}

/// A broadcast one node rejects and another cannot answer still succeeds on the third, marks
/// the transaction `ours` so a wallet sees it before quorum, and is a rejection only when every
/// node rejects it.
#[tokio::test]
async fn a_mixed_broadcast_succeeds_and_ours_is_servable_before_quorum() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let agreed = BlockRef {
        hash: BlockHash::from([50u8; 32]),
        height: Height::try_from(50).expect("50 is in range"),
    };
    for validator in &validators {
        validator.edit(|fake| fake.tip = Some(agreed));
    }
    validators[1].edit(|fake| {
        fake.relay = Some(Err(SendRawTransactionError::Rejected(
            "tx unpaid action limit exceeded".to_string(),
        )))
    });
    validators[2].edit(|fake| fake.relay_unreachable = true);

    let (view, pollers) = ChainView::new(
        validators
            .iter()
            .zip(["a:8232", "b:8232", "c:8232"])
            .map(|(source, address)| Endpoint {
                address: address.to_owned(),
                source: Arc::clone(source),
            })
            .collect(),
        depth(),
    )
    .expect("three endpoints is a valid set");
    let reader = view.subscriber();

    pollers[0].tick().await.expect("endpoint a polls");
    pollers[1].tick().await.expect("endpoint b polls");
    assert!(reader.current().tip().is_some(), "a and b agree on a tip");

    let txid = view
        .broadcast(vec![9u8; 8])
        .await
        .expect("one accept is enough; a rejecting node has a stricter local policy");
    assert_eq!(txid, TransactionId::from([9u8; 32]));

    let pinned = reader.current();
    let sighting = pinned.sighting(&txid).expect("the relay recorded it");
    assert!(sighting.ours());
    assert!(sighting.seen_at().is_empty(), "no endpoint listed it yet (not propagated)");
    let mempool = pinned.mempool().expect("quorum met");
    let served = mempool.get(&txid).expect("`ours` is servable with zero sightings");
    let unpriced = MempoolEntry { txid, raw: Bytes::from(vec![9u8; 8]), fee: None };
    assert_eq!(served, unpriced, "a wallet sees its own send before it propagates, unpriced");

    // Propagated: the first listing prices it (bytes already held, so none refetched)
    validators[0].edit(|fake| {
        fake.listed.insert(txid);
    });
    pollers[0].tick().await.expect("endpoint a lists it");
    let pinned = reader.current();
    let listed = pinned.mempool().expect("quorum met").get(&txid);
    let priced = Some((Bytes::from(vec![9u8; 8]), Some(fee_of(&txid))));
    assert_eq!(listed.map(|entry| (entry.raw, entry.fee)), priced);

    // Unanimous domain rejection = the real one
    for validator in &validators {
        validator.edit(|fake| {
            fake.relay = Some(Err(SendRawTransactionError::Rejected("too low fee".to_string())));
            fake.relay_unreachable = false;
        });
    }
    let sent = view.broadcast(vec![8u8; 8]).await;
    assert!(matches!(sent, Err(BroadcastError::Rejected(SendRawTransactionError::Rejected(_)))));

    // None reachable != a rejection (nothing learnt about the transaction)
    for validator in &validators {
        validator.edit(|fake| fake.relay_unreachable = true);
    }
    let sent = view.broadcast(vec![8u8; 8]).await;
    assert!(matches!(sent, Err(BroadcastError::Unreachable { attempted: 3, .. })), "{sent:?}");
}

/// - One-block race → quorum + open tails kept; lagging majority → retreat onto the ancestor
/// - Agreers-only change → tip watch moves, epoch doesn't; 1 voter of 3 → shortfall count 1
#[tokio::test]
async fn ancestry_votes_ride_a_propagation_race_and_retreat_onto_a_lagging_majority() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let at = |h: u32| {
        let height = Height::try_from(h).expect("h");
        BlockRef { hash: trunk(height), height }
    };
    for validator in &validators {
        validator.edit(|fake| fake.tip = Some(at(100)));
    }
    let (view, pollers) = ChainView::new(
        validators
            .iter()
            .zip(["a:8232", "b:8232", "c:8232"])
            .map(|(source, address)| Endpoint {
                address: address.to_owned(),
                source: Arc::clone(source),
            })
            .collect(),
        depth(),
    )
    .expect("three endpoints is a valid set");
    let reader = view.subscriber();
    let mut tips = reader.subscribe_tip();
    let agreed_by = |positions: &[usize]| EndpointSet::at(positions.iter().copied());
    let agreements = || -> Vec<Agreement> {
        reader.current().endpoints().iter().map(|meta| meta.agreement).collect()
    };
    async fn open(tail: &mut crate::MempoolTail) -> bool {
        tokio::time::timeout(Duration::from_millis(20), tail.next()).await.is_err()
    }
    for poller in &pollers {
        poller.tick().await.expect("polls");
    }
    let settled = reader.current().tip().expect("three agree");
    assert_eq!((settled.block, settled.agreed_by), (at(100), agreed_by(&[0, 1, 2])));
    let mut first = reader.tail().expect("quorum met");

    // a mines 101 first: 100 is still on all three chains (exact-tip voting split here)
    validators[0].edit(|fake| fake.tip = Some(at(101)));
    pollers[0].tick().await.expect("a polls");
    let race = reader.current().tip().expect("race keeps the quorum");
    assert_eq!((race.block, race.agreed_by), (at(100), agreed_by(&[0, 1, 2])));
    assert!(reader.current().mempool().is_ok(), "never UNAVAILABLE mid-race");
    assert_eq!(agreements(), [Agreement::Ahead, Agreement::Agreed, Agreement::Agreed]);
    assert!(open(&mut first).await, "same block: the stream stays open");

    // b follows: 101 has a majority, the stream ends, c trails
    validators[1].edit(|fake| fake.tip = Some(at(101)));
    pollers[1].tick().await.expect("b polls");
    let moved = reader.current().tip().expect("two of three hold 101");
    assert_eq!((moved.block, moved.agreed_by), (at(101), agreed_by(&[0, 1])));
    assert_eq!(agreements(), [Agreement::Agreed, Agreement::Agreed, Agreement::Behind]);
    assert_eq!(first.next().await, None, "a new block ends the stream");
    let mut second = reader.tail().expect("quorum met");

    // a stops voting: 101 is held by b alone, so the tip retreats onto the common ancestor
    validators[0].edit(|fake| fake.not_ready = true);
    assert_eq!(pollers[0].tick().await.expect("a answers"), Polled::Syncing);
    let retreat = reader.current().tip().expect("b and c share 100");
    assert_eq!((retreat.block, retreat.agreed_by), (at(100), agreed_by(&[1, 2])));
    assert_eq!(agreements(), [Agreement::Ahead, Agreement::Ahead, Agreement::Agreed]);
    assert_eq!(second.next().await, None, "a retreat is a tip move too");

    // c catches up, then a returns: the second change is agreers-only
    validators[2].edit(|fake| fake.tip = Some(at(101)));
    pollers[2].tick().await.expect("c polls");
    let mut third = reader.tail().expect("quorum met");
    tips.borrow_and_update();
    validators[0].edit(|fake| fake.not_ready = false);
    pollers[0].tick().await.expect("a polls again");
    let joined = *tips.borrow_and_update();
    let joined = joined.expect("quorum");
    assert_eq!((joined.block, joined.agreed_by), (at(101), agreed_by(&[0, 1, 2])));
    assert!(open(&mut third).await, "agreers-only change: same epoch, stream open");

    // b and c stop voting: one of three is no quorum, and the refusal counts that one
    for (validator, poller) in validators.iter().zip(&pollers).skip(1) {
        validator.edit(|fake| fake.not_ready = true);
        poller.tick().await.expect("answers");
    }
    let refused = reader.current().mempool().err();
    assert_eq!(refused, Some(BelowQuorum { agreeing: 1, threshold: 2, configured: 3 }));
    assert_eq!(*tips.borrow(), None);
}

/// - Reorg between tip read and header reads → last chain kept, next tick walks to the fork
/// - Jump past the window → full rebuild
#[tokio::test]
async fn each_block_costs_one_header_and_a_mid_walk_reorg_keeps_the_last_chain() {
    let validator = Arc::new(FakeValidator::default());
    let at = |h: u32| {
        let height = Height::try_from(h).expect("h");
        BlockRef { hash: trunk(height), height }
    };
    validator.edit(|fake| fake.tip = Some(at(10)));
    let (view, pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    let reader = view.subscriber();
    let served = || validator.0.lock().expect("fake validator mutex poisoned").links_served;
    let voted = || reader.current().endpoints()[0].tip();

    pollers[0].tick().await.expect("first poll");
    assert_eq!((voted(), served()), (Some(at(10)), 3), "first build = depth headers");
    validator.edit(|fake| fake.tip = Some(at(11)));
    pollers[0].tick().await.expect("next block");
    pollers[0].tick().await.expect("same block");
    assert_eq!((voted(), served()), (Some(at(11)), 4), "one header, then none");

    let fork_12 = BlockRef { hash: BlockHash::from([0xab; 32]), height: at(12).height };
    let fork_11 = BlockHash::from([0xac; 32]);
    validator.edit(|fake| {
        fake.tip = Some(fork_12);
        fake.branch.insert(at(11).height, fork_11);
        fake.links_from = Some([(at(12).height, BlockHash::from([0xad; 32]))].into());
    });
    let raced = pollers[0].tick().await.expect("a race is not a failure");
    assert_eq!(raced, Polled::Listed(0));
    let pinned = reader.current();
    let meta = &pinned.endpoints()[0];
    assert_eq!((meta.tip(), meta.state), (Some(at(11)), EndpointState::Live), "last chain kept");

    validator.edit(|fake| fake.links_from = None);
    let before = served();
    pollers[0].tick().await.expect("settled");
    let cost = served() - before;
    assert_eq!(voted(), Some(fork_12));
    assert!((2..=3).contains(&cost), "12', then 11' joins (10 may ride its batch): {cost}");
    assert_eq!(reader.current().tip().map(|tip| tip.block), Some(fork_12));

    validator.edit(|fake| {
        fake.branch.clear();
        fake.tip = Some(at(40));
    });
    let before = served();
    pollers[0].tick().await.expect("jump");
    assert_eq!((voted(), served() - before), (Some(at(40)), 3), "past the window = a rebuild");
}
