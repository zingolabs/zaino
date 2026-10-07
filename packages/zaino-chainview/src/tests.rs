//! Scenarios against fake endpoints: a `MockChain` serves each one's real chain (header bytes,
//! linkage, `getblockhash`, reachability); [`FakeValidator`] adds what these tests are about
//! (mempool listings and fees, metadata failures, relay verdicts, a reorg between its tip read and
//! its `getblockhash` answers, counters)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use zaino_header_chain::VerifiedChain;
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{
    Block, BlockHash, BlockRef, EndOfService, Height, NodeRelease, PeerInfo, ReorgDepth,
    TransactionId, Zatoshis,
};
use zaino_source::mock::MockChain;
use zaino_source::{
    BlockLinks, ChainDataSource, FailureMode, GetBlockByHashError, GetBlockError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, NonDomainError, PollReading, QueryError, RawMempoolTransactions,
    SendRawTransactionError, TransactionResponse,
};

use crate::endpoint::Polled;
use crate::{
    Agreement, ChainView, Count, Endpoint, EndpointSet, EndpointState, MempoolEntry, Projection,
    SubmitError, Unserved,
};

/// What one validator answers beyond its chain, mutated between ticks by the test
#[derive(Default)]
struct FakeState {
    mempool_inactive: bool,
    /// `None` = at the tip it reports
    network_tip: Option<Height>,
    listed: BTreeSet<TransactionId>,
    bytes: BTreeMap<TransactionId, Vec<u8>>,
    peers: Vec<PeerInfo>,
    peers_unreachable: bool,
    /// `None` = the release read times out
    release: Option<NodeRelease>,
    polls: usize,
    /// Best chain after the next poll's tip read (reorged before its `getblockhash` answers)
    reorg_after_poll: Option<Vec<Block>>,
    links_served: usize,
    /// `None` accepts (lists it from its next poll); `Some(Err)` = its rejection
    relay: Option<Result<(), SendRawTransactionError>>,
    relay_unreachable: bool,
    pushes: usize,
}

#[derive(Default)]
struct FakeValidator {
    chain: MockChain,
    state: Mutex<FakeState>,
}

impl FakeValidator {
    fn edit<T>(&self, edit: impl FnOnce(&mut FakeState) -> T) -> T {
        edit(&mut self.state.lock().expect("fake validator mutex poisoned"))
    }

    fn read<T>(&self, read: impl FnOnce(&FakeState) -> T) -> T {
        read(&self.state.lock().expect("fake validator mutex poisoned"))
    }

    /// Best chain = `chain`'s genesis ..= `tip`
    fn serve(&self, chain: &Chain, tip: BlockRef) {
        self.chain.extend_best(chain.path(tip.hash));
    }
}

/// Fee a fake lists for `txid` (a function of the tx, as on a real validator)
fn fee_of(txid: &TransactionId) -> Zatoshis {
    Zatoshis::new(u64::from(<[u8; 32]>::from(*txid)[0]) * 1_000).expect("in supply")
}

fn outbound(addr: &str) -> PeerInfo {
    PeerInfo { addr: addr.to_owned(), inbound: false }
}

/// Finality depth: polls ask who holds 3 below the best
fn depth() -> ReorgDepth {
    ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"))
}

/// Header sync's word on `chain`'s path to `tip`
fn verified(chain: &Chain, tip: BlockRef) -> Option<VerifiedChain> {
    Some(VerifiedChain::regtest(&chain.path(tip.hash)))
}

/// What the view asks: links, tip + `getblockhash` (its `MockChain`; reachability too), polls
/// (estimate, listing with each tx's fee, metadata = peers and the release as set), mempool bytes,
/// sends (accepting = into its own mempool, listed from its next poll, as a zebrad does)
impl ChainDataSource for FakeValidator {
    async fn get_block(&self, _: Height) -> Result<Block, QueryError<GetBlockError>> {
        unimplemented!("the view fetches no blocks")
    }

    async fn get_block_by_hash(
        &self,
        _: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        unimplemented!("the view fetches no blocks")
    }

    async fn get_transaction(
        &self,
        _: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        unimplemented!("the view looks up no mined transaction")
    }

    async fn get_block_links(&self, heights: &[Height]) -> Result<BlockLinks, NonDomainError> {
        self.edit(|fake| fake.links_served += heights.len());
        self.chain.get_block_links(heights).await
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.edit(|fake| fake.polls += 1);
        let PollReading { mut info, .. } = self.chain.get_poll_reading(false, &[]).await?;
        if let Some(reorged) = self.edit(|fake| fake.reorg_after_poll.take()) {
            self.chain.extend_best(reorged);
        }
        let held = self.chain.get_poll_reading(false, holds).await?.held;
        let fake = self.state.lock().expect("fake validator mutex poisoned");
        info.estimated_height = fake.network_tip.unwrap_or(info.blocks);
        let listing = match fake.mempool_inactive {
            true => Err(GetMempoolListingError::Inactive),
            false => Ok(fake
                .listed
                .iter()
                .map(|txid| MempoolListed { txid: *txid, fee: fee_of(txid), encoded_len: 8 })
                .collect()),
        };
        let timed_out = || NonDomainError::new(FailureMode::Timeout, "fake metadata timed out");
        let metadata = metadata.then(|| MetadataReading {
            peers: match fake.peers_unreachable {
                true => Err(timed_out()),
                false => Ok(fake.peers.clone()),
            },
            release: fake.release.clone().ok_or_else(timed_out),
        });
        Ok(PollReading { info, listing, held, metadata })
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        let fake = self.state.lock().expect("fake validator mutex poisoned");
        Ok(listed
            .iter()
            .map(|entry| {
                let held = fake.bytes.get(&entry.txid).cloned();
                held.ok_or(GetRawMempoolTransactionError::NotFound(entry.txid))
            })
            .collect())
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        let mut fake = self.state.lock().expect("fake validator mutex poisoned");
        fake.pushes += 1;
        if fake.relay_unreachable {
            return Err(QueryError::NonDomain(NonDomainError::new(
                FailureMode::Connection,
                "fake validator unreachable",
            )));
        }
        if let Some(Err(rejected)) = &fake.relay {
            return Err(QueryError::Domain(rejected.clone()));
        }
        let txid =
            zaino_source::prepare_transaction(&transaction).expect("tests push real txs").txid;
        fake.listed.insert(txid);
        fake.bytes.insert(txid, transaction);
        Ok(txid)
    }
}

/// A real, empty v4 transaction (no bundles), distinct per `lock_time`, and its txid
fn transaction(lock_time: u32, expiry: u32) -> (TransactionId, Vec<u8>) {
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V4,
        zcash_protocol::consensus::BranchId::Canopy,
        lock_time,
        expiry.into(),
        None,
        None,
        None,
        None,
    )
    .freeze()
    .expect("v4 freezes");
    let mut raw = Vec::new();
    tx.write(&mut raw).expect("writes");
    (TransactionId::from(*tx.txid().as_ref()), raw)
}

/// N=1, its window holds the verified tip. A tail = the servable mempool at its tip block, then each later
/// crossing once (never one the opening carried), silent on an empty mempool, ended by a mined
/// block; a late subscriber gets the same opening + every arrival since (one log per block)
#[tokio::test]
async fn a_single_endpoint_tail_sends_the_mempool_at_its_block_then_each_arrival_once() {
    let mut chain = Chain::new();
    let tip_10 = chain.extend(chain.genesis().hash, 10);
    let validator = Arc::new(FakeValidator::default());
    validator.serve(&chain, tip_10);
    validator.edit(|fake| {
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

    assert_eq!(reader.current().mempool().err(), Some(Unserved::NoBestTip), "no verified tip yet");
    assert!(reader.tail().is_err(), "no tip: the stream is refused, not opened silent");
    view.set_verified(verified(&chain, tip_10));
    let unheld = reader.current().mempool().err();
    assert_eq!(unheld, Some(Unserved::NotHeld { height: 10, configured: 1 }), "nothing polled");

    assert_eq!(pollers[0].tick().await.expect("first poll succeeds"), Polled::Listed(2));
    let pinned = reader.current();
    let tip = pinned.tip().expect("the validator holds the verified tip");
    assert_eq!((tip.block, tip.held_by), (tip_10, EndpointSet::at([0])));
    let entry = |seed: u8, fee: u64| MempoolEntry {
        txid: TransactionId::from([seed; 32]),
        raw: Bytes::from(vec![seed; 8]),
        fee: Some(Zatoshis::new(fee).expect("in supply")),
        projection: Projection::default(),
    };
    let entries: Vec<_> = pinned.mempool().expect("tip held").entries().collect();
    assert_eq!(entries, [entry(1, 1_000), entry(2, 2_000)], "each entry: its validator's fee");

    let mut tail = reader.tail().expect("tip held");
    assert_eq!(tail.opening(), [entry(1, 1_000), entry(2, 2_000)], "the whole servable mempool");

    // tx 2 flaps out and back: it rode the opening, so its re-crossing is not logged
    // tx 1 is dropped (propagation churn): nothing to send
    // tx 3 arrives, and ours (tx 9) is servable before any listing
    validator.edit(|fake| fake.listed.retain(|txid| *txid != TransactionId::from([2u8; 32])));
    pollers[0].tick().await.expect("second poll succeeds");
    validator.edit(|fake| {
        fake.listed = [2u8, 3].map(|seed| TransactionId::from([seed; 32])).into_iter().collect();
        fake.bytes.insert(TransactionId::from([3u8; 32]), vec![3u8; 8]);
    });
    pollers[0].tick().await.expect("third poll succeeds");
    let (txid, sent) = transaction(9, 0);
    let ours = view.submit(sent.clone()).await.expect("accepted");
    assert_eq!(ours, txid, "the txid from the bytes, not the validator's word");

    async fn next(tail: &mut crate::MempoolTail) -> Option<MempoolEntry> {
        tail.next().await.map(|logged| logged.entry.clone())
    }
    let raw = Bytes::from(sent);
    let projection = Projection::default();
    let unpriced = MempoolEntry { txid: ours, raw, fee: None, projection };
    assert_eq!(next(&mut tail).await, Some(entry(3, 3_000)), "tx 2 rode the opening: skipped");
    assert_eq!(next(&mut tail).await, Some(unpriced.clone()), "our own send, before any listing");
    let silent = tokio::time::timeout(Duration::from_millis(50), tail.next()).await;
    assert!(silent.is_err(), "nothing new, no block mined: a live, silent stream");

    // A late subscriber: the same opening (tx 1 included: the stream never un-sends within a
    // block) + every arrival since, from the same log
    let mut late = reader.tail().expect("tip held");
    assert!(late.same_epoch(&tail), "one log per tip block");
    assert_eq!(late.opening(), [entry(1, 1_000), entry(2, 2_000)]);
    assert_eq!(next(&mut late).await, Some(entry(3, 3_000)));
    assert_eq!(next(&mut late).await, Some(unpriced));

    // Block 11: the one thing that ends a stream, for every tail on the old tip
    let tip_11 = chain.mine(tip_10.hash);
    validator.serve(&chain, tip_11);
    pollers[0].tick().await.expect("fourth poll succeeds");
    view.set_verified(verified(&chain, tip_11));
    assert!(tail.next().await.is_none(), "the stream ends on a mined block");
    assert!(late.next().await.is_none(), "late subscriber too");
    assert!(tail.next().await.is_none(), "and stays ended");
    let fresh = reader.tail().expect("tip held");
    let opened: Vec<_> = fresh.opening().iter().map(|entry| entry.txid).collect();
    let mut current = [2u8, 3].map(|seed| TransactionId::from([seed; 32])).to_vec();
    current.push(ours);
    current.sort();
    assert_eq!(opened, current, "the new tip opens on the mempool as it now stands (ours listed)");
}

/// A validator whose mempool is off below the network tip still holds the verified tip (the
/// sync producer's input) and serves an empty mempool, then lists again once active.
#[tokio::test]
async fn a_catching_up_validator_holds_the_tip_with_no_mempool() {
    let mut chain = Chain::new();
    let tip_12 = chain.extend(chain.genesis().hash, 12);
    let path = chain.path(tip_12.hash);
    let tip_at = |height: usize| BlockRef {
        hash: path[height].header().hash,
        height: path[height].header().height,
    };
    let network = Height::try_from(40).expect("in range");
    let validator = Arc::new(FakeValidator::default());
    validator.serve(&chain, tip_at(10));
    validator.edit(|fake| {
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
    view.set_verified(verified(&chain, tip_at(10)));

    validator.serve(&chain, tip_at(11));
    validator.edit(|fake| {
        fake.network_tip = Some(network);
        fake.mempool_inactive = true;
    });
    assert_eq!(
        pollers[0].tick().await.expect("an inactive mempool is an answer, not a failure"),
        Polled::CatchingUp { tip: tip_at(11), network },
    );
    view.set_verified(verified(&chain, tip_at(11)));
    assert_eq!(tip.borrow().map(|tip| tip.block), Some(tip_at(11)), "its chain still holds it");
    let catching_up = reader.current();
    assert_eq!(catching_up.endpoints()[0].state, EndpointState::CatchingUp);
    assert_eq!(catching_up.endpoints()[0].failures, 0);
    assert_eq!(
        catching_up.mempool().expect("a holder of the verified tip").entries().count(),
        0,
        "sighting retracted with the mempool off",
    );

    validator.serve(&chain, tip_12);
    validator.edit(|fake| {
        fake.network_tip = None;
        fake.mempool_inactive = false;
        fake.listed = [TransactionId::from([2u8; 32])].into_iter().collect();
        fake.bytes = [(TransactionId::from([2u8; 32]), vec![2u8; 8])].into_iter().collect();
    });
    assert_eq!(pollers[0].tick().await.expect("caught-up poll succeeds"), Polled::Listed(1));
    view.set_verified(verified(&chain, tip_at(12)));
    let caught_up = reader.current();
    assert_eq!(caught_up.endpoints()[0].state, EndpointState::Live);
    assert_eq!(
        caught_up.mempool().expect("held").entries().map(|e| e.txid).collect::<Vec<_>>(),
        [TransactionId::from([2u8; 32])],
    );
}

/// The tip is the verified best block, never a validator's claim: holders = every validator whose
/// chain holds it (one is enough); a third claiming a far higher tip moves nothing, drops out of
/// the holders, and never supplies the chain description served
#[tokio::test]
async fn a_claimed_higher_tip_moves_nothing_and_holders_are_whoever_holds_the_verified_block() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let mut chain = Chain::new();
    let at_90 = chain.extend(chain.genesis().hash, 90);
    let agreed = chain.extend(at_90.hash, 10);
    let tx7 = TransactionId::from([7u8; 32]);
    for (validator, estimate) in validators.iter().zip([105, 106, 107]) {
        validator.serve(&chain, agreed);
        validator.edit(|fake| {
            fake.network_tip = Some(Height::try_from(estimate).expect("in range"));
            fake.bytes = [(tx7, vec![7u8; 8])].into_iter().collect();
            fake.peers = vec![outbound("seed-a:8233")];
        });
    }
    let estimate = |pinned: &crate::ChainViewSnapshot| {
        pinned.validator_info().map(|info| u32::from(info.estimated_height))
    };
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
    view.set_verified(verified(&chain, agreed));

    // One validator holding the verified block = a tip (one admission proves validity)
    pollers[0].tick().await.expect("endpoint a polls");
    let pinned = reader.current();
    let tip = pinned.tip().expect("a holds the verified block");
    assert_eq!((tip.block, tip.held_by), (agreed, EndpointSet::at([0])));
    assert_eq!(estimate(&pinned), Ok(105), "the holder's chain description");
    let spread = pinned.spread(&tx7).expect("endpoint a reported it");
    let one_of_one = Count { seen: 1, of: 1 };
    assert_eq!(spread.trusted, one_of_one, "b, c not yet read");
    assert!(spread.timeline.all_trusted.is_some(), "every reader so far lists it");
    let mut tail = reader.tail().expect("a tip");
    let opened: Vec<_> = tail.opening().iter().map(|entry| entry.txid).collect();
    assert_eq!(opened, [tx7], "in the opening of the first block with a tip");

    pollers[1].tick().await.expect("endpoint b polls");
    let tip = reader.current().tip().expect("a and b hold it");
    assert_eq!(tip.held_by, EndpointSet::at([0, 1]));

    // Endpoint c claims a higher tip (a fork from 90, never verified): moves nothing, holds nothing
    let claimed = chain.extend(at_90.hash, 30);
    validators[2].serve(&chain, claimed);
    validators[2].edit(|fake| {
        fake.listed = [tx7].into_iter().collect();
        fake.peers = vec![outbound("seed-z:8233")];
    });
    pollers[2].tick().await.expect("endpoint c polls");
    let pinned = reader.current();
    let tip = pinned.tip().expect("a and b still hold it");
    assert_eq!((tip.block, tip.held_by), (agreed, EndpointSet::at([0, 1])), "never the claim");
    assert_eq!(pinned.endpoints()[2].agreement, Agreement::Diverged, "holds neither 100 nor 97");
    assert_eq!(estimate(&pinned), Ok(105), "never the outlier's");
    let trusted = pinned.spread(&tx7).map(|spread| spread.trusted);
    assert_eq!(trusted, Some(Count { seen: 2, of: 3 }), "a and c list it, of three read");
    let peers: Vec<(&str, Vec<PeerInfo>)> = pinned
        .endpoints()
        .iter()
        .map(|meta| (meta.address.as_str(), meta.peers.iter().cloned().collect()))
        .collect();
    let expected =
        [("a:8232", "seed-a:8233"), ("b:8232", "seed-a:8233"), ("c:8232", "seed-z:8233")]
            .map(|(address, peer)| (address, vec![outbound(peer)]));
    assert_eq!(peers, expected, "each validator's peers, keyed by its configured address");

    let again = tokio::time::timeout(Duration::from_millis(20), tail.next()).await;
    assert!(again.is_err(), "a second sighting spreads it, never re-sends it");
}

/// One validator rejects, one is unreachable, one accepts: random entries until one accepts,
/// each pushed at most once, and the accepted transaction is `ours` (servable before any
/// listing, unpriced); a unanimous rejection is the rejection, none reachable is no answer, and
/// an expired transaction is refused with no push at all
#[tokio::test]
async fn a_submission_tries_random_entries_until_one_accepts_and_ours_is_servable_at_once() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let mut chain = Chain::new();
    let agreed = chain.extend(chain.genesis().hash, 50);
    for validator in &validators {
        validator.serve(&chain, agreed);
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
    view.set_verified(verified(&chain, agreed));
    assert!(reader.current().tip().is_some(), "a and b hold the verified tip");
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.read(|f| f.pushes)).collect() };

    let (expired_txid, expired) = transaction(1, 50);
    let refused = view.submit(expired).await;
    let Err(SubmitError::Rejected(SendRawTransactionError::Rejected(why))) = refused else {
        panic!("expiry 50 at tip 50 = invalid in block 51: {refused:?}");
    };
    assert!(why.contains("expiry height 50"), "{why}");
    assert_eq!(pushes(), [0, 0, 0], "precheck refuses before any push");
    assert!(reader.current().spread(&expired_txid).is_none());

    let (txid, sent) = transaction(9, 0);
    let accepted = view.submit(sent.clone()).await;
    assert_eq!(accepted.expect("one accept is enough; a refusal may be local policy"), txid);
    let tried = pushes();
    assert_eq!(tried[0], 1, "the accepting validator, once");
    assert!(tried.iter().all(|&n| n <= 1), "no validator pushed twice: {tried:?}");

    let pinned = reader.current();
    let spread = pinned.spread(&txid).expect("the submission recorded it");
    assert!(spread.ours && spread.servable);
    assert_eq!(
        spread.trusted,
        Count { seen: 0, of: 2 },
        "not polled since; a and b read, c not yet"
    );
    assert_eq!(spread.timeline.first_trusted, None);
    let mempool = pinned.mempool().expect("tip held");
    let served = mempool.get(&txid).expect("`ours` is servable with zero sightings");
    let raw = Bytes::from(sent.clone());
    let unpriced = MempoolEntry { txid, raw, fee: None, projection: Projection::default() };
    assert_eq!(served, unpriced, "a wallet sees its own send before it propagates, unpriced");
    let render = |as_: &'static [u8]| move || Ok::<_, ()>(Bytes::from_static(as_));
    assert_eq!(
        served.projection.get_or_render(render(b"unpriced")),
        Ok(Bytes::from_static(b"unpriced"))
    );
    let again = reader.current().mempool().expect("tip held").get(&txid).expect("held");
    let cached = again.projection.get_or_render(render(b"rendered twice"));
    assert_eq!(cached, Ok(Bytes::from_static(b"unpriced")), "one render per (raw, fee)");

    // a lists what it accepted: the first listing prices it (bytes held, none refetched)
    pollers[0].tick().await.expect("endpoint a lists it");
    let pinned = reader.current();
    let listed = pinned.mempool().expect("tip held").get(&txid);
    let listed = listed.expect("still servable");
    let rerendered = listed.projection.get_or_render(render(b"priced"));
    assert_eq!(rerendered, Ok(Bytes::from_static(b"priced")), "priced → stale projection dropped");
    let priced = (Bytes::from(sent), Some(fee_of(&txid)));
    assert_eq!((listed.raw, listed.fee), priced);
    let spread = pinned.spread(&txid).expect("held");
    assert_eq!(spread.trusted, Count { seen: 1, of: 2 });
    let timeline = spread.timeline;
    assert!(timeline.first_trusted.is_some_and(|at| at >= timeline.first_seen), "{timeline:?}");
    assert_eq!(timeline.all_trusted, None, "b has not listed it");

    // Unanimous domain rejection = the real one, after every validator was tried once
    for validator in &validators {
        validator.edit(|fake| {
            fake.relay = Some(Err(SendRawTransactionError::Rejected("too low fee".to_string())));
            fake.relay_unreachable = false;
            fake.pushes = 0;
        });
    }
    let rejected = view.submit(transaction(8, 0).1).await;
    assert!(matches!(rejected, Err(SubmitError::Rejected(SendRawTransactionError::Rejected(_)))));
    assert_eq!(pushes(), [1, 1, 1]);

    // None reachable != a rejection (nothing learnt about the transaction)
    for validator in &validators {
        validator.edit(|fake| fake.relay_unreachable = true);
    }
    let sent = view.submit(transaction(7, 0).1).await;
    assert!(matches!(sent, Err(SubmitError::Unreachable { attempted: 3, .. })), "{sent:?}");

    let garbage = view.submit(vec![9u8; 8]).await;
    assert!(matches!(garbage, Err(SubmitError::Rejected(SendRawTransactionError::Malformed(_)))));
}

/// Paused clock, every validator accepting, none gossiping on its own: the lone entry's listing
/// is no spread, so after the threshold the job resubmits to another; once a validator that was
/// never an entry lists it, the job ends and pushes nothing more
#[tokio::test(start_paused = true)]
async fn an_unspread_submission_is_resubmitted_after_the_threshold_until_an_outsider_lists_it() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let chain = Chain::new();
    for validator in &validators {
        validator.serve(&chain, chain.genesis());
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
    .expect("three endpoints");
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(4).expect("nz"),
    };
    let view = view.with_submit_policy(policy);
    for poller in &pollers {
        poller.tick().await.expect("polls");
    }
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.read(|f| f.pushes)).collect() };
    async fn poll_all(pollers: &[crate::EndpointPoller<FakeValidator>]) {
        for poller in pollers {
            poller.tick().await.expect("polls");
        }
    }

    let (txid, sent) = transaction(5, 0);
    view.submit(sent).await.expect("the first entry accepts");
    let first = pushes();
    assert_eq!(first.iter().sum::<usize>(), 1);
    poll_all(&pollers).await;
    assert_eq!(reader_trusted(&view, txid), Count { seen: 1, of: 3 }, "the entry lists it");

    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(pushes(), first, "inside the threshold: no resubmit");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = pushes();
    assert_eq!(second.iter().sum::<usize>(), 2, "threshold passed unspread: a second entry");
    assert!(second.iter().all(|&n| n <= 1), "a different validator: {second:?}");

    // the one validator never pushed to hears it by gossip and lists it: spread
    let outsider = second.iter().position(|&n| n == 0).expect("one untried");
    validators[outsider].edit(|fake| {
        fake.listed.insert(txid);
    });
    poll_all(&pollers).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(pushes(), second, "spread past its entries: the job ended, nothing more pushed");
    assert_eq!(reader_trusted(&view, txid), Count { seen: 3, of: 3 });
}

fn reader_trusted(view: &ChainView<FakeValidator>, txid: TransactionId) -> Count {
    view.subscriber().current().spread(&txid).expect("held").trusted
}

/// The p2p layer as a script: announcements the test sends, live peers, pushes recorded (a
/// peer answers nothing; `dead` ones refuse the connection)
struct FakePeers {
    live: Vec<std::net::SocketAddr>,
    dead: BTreeSet<std::net::SocketAddr>,
    pushes: Mutex<Vec<std::net::SocketAddr>>,
    announce: tokio::sync::broadcast::Sender<crate::Heard>,
}

impl crate::ValidatorP2pSource for FakePeers {
    fn heard(&self) -> futures::stream::BoxStream<'static, crate::Heard> {
        use futures::StreamExt;
        let rx = self.announce.subscribe();
        futures::stream::unfold(rx, |mut rx| async move { Some((rx.recv().await.ok()?, rx)) })
            .boxed()
    }

    fn live(&self) -> Vec<std::net::SocketAddr> {
        self.live.clone()
    }

    fn entries(&self, _: Option<Height>) -> Vec<std::net::SocketAddr> {
        self.live.clone()
    }

    fn push(
        &self,
        entry: std::net::SocketAddr,
        _: Bytes,
    ) -> futures::future::BoxFuture<'static, Result<(), NonDomainError>> {
        self.pushes.lock().expect("fake peers mutex poisoned").push(entry);
        let refused = self.dead.contains(&entry);
        Box::pin(std::future::ready(match refused {
            true => Err(NonDomainError::new(FailureMode::Connection, "fake peer refused")),
            false => Ok(()),
        }))
    }
}

/// Paused clock, two trusted validators, four live peers in four netgroups + one dead:
/// - announcements before any trusted listing are overheard, then join the sighting: `peers:
///   2/4, trusted: 1/2`, first seen = the first announcement
/// - a submission goes to a peer, never a trusted validator first; a black hole is waited out,
///   the next entry is another netgroup; an outside announcer + a trusted listing = the wallet's
///   answer, with no trusted validator ever pushed to
/// - every peer a black hole (or dead): the budget spent on peers, then one trusted verdict
#[tokio::test(start_paused = true)]
async fn peers_are_heard_first_entries_first_and_a_trusted_validator_gives_the_verdict() {
    use crate::Heard;
    let validators: Vec<Arc<FakeValidator>> =
        (0..2).map(|_| Arc::new(FakeValidator::default())).collect();
    let chain = Chain::new();
    for validator in &validators {
        validator.serve(&chain, chain.genesis());
    }
    let (view, pollers) = ChainView::new(
        validators
            .iter()
            .zip(["a:8232", "b:8232"])
            .map(|(source, address)| Endpoint {
                address: address.to_owned(),
                source: Arc::clone(source),
            })
            .collect(),
        depth(),
    )
    .expect("two endpoints");
    let peer = |b: u8| std::net::SocketAddr::from(([10, b, 0, 1], 8233));
    let (announce, _) = tokio::sync::broadcast::channel::<Heard>(64);
    let peers = Arc::new(FakePeers {
        live: (0..4).map(peer).collect(),
        dead: BTreeSet::from([peer(9)]),
        pushes: Mutex::new(Vec::new()),
        announce: announce.clone(),
    });
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
    tokio::task::yield_now().await; // the watch subscribes before the first announcement
    let poll_all = || async {
        for poller in &pollers {
            poller.tick().await.expect("polls");
        }
    };
    let fold = || tokio::time::sleep(crate::peers::PEER_FOLD * 2);
    let validator_pushes = || validators.iter().map(|v| v.read(|f| f.pushes)).sum::<usize>();
    let peer_pushes = || peers.pushes.lock().expect("fake peers mutex poisoned").clone();
    poll_all().await;

    // heard first: overheard until a trusted validator lists it
    let (gossiped, raw) = transaction(1, 0);
    let first_heard = tokio::time::Instant::now();
    announce.send(Heard { peer: peer(1), txids: vec![gossiped] }).expect("watch running");
    announce.send(Heard { peer: peer(2), txids: vec![gossiped] }).expect("watch running");
    fold().await;
    assert_eq!(view.subscriber().current().spread(&gossiped), None, "peer-only = not held");
    tokio::time::sleep(Duration::from_secs(3)).await;
    validators[0].edit(|fake| {
        fake.listed.insert(gossiped);
        fake.bytes.insert(gossiped, raw);
    });
    poll_all().await;
    let spread = view.subscriber().current().spread(&gossiped).expect("held once listed");
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 2, of: 4 }, Count { seen: 1, of: 2 })
    );
    assert_eq!(spread.timeline.first_seen, first_heard, "first seen = the first announcement");
    assert!(spread.timeline.first_trusted > Some(first_heard));
    assert_eq!(view.subscriber().current().peers_live.len(), 4);

    // submitted through peers: a black hole waited out, a relay answers the wallet
    let (txid, sent) = transaction(2, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::task::yield_now().await;
    let first = peer_pushes();
    assert_eq!(
        (first.len(), validator_pushes()),
        (1, 0),
        "one peer entry, no validator: {first:?}"
    );
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(peer_pushes().len(), 1, "inside the threshold: no resubmit");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = peer_pushes();
    assert_eq!(second.len(), 2, "threshold passed unseen: a second peer");
    let group = crate::submit::Netgroup::of;
    assert_ne!(group(second[0]), group(second[1]), "another netgroup");
    let outsider = (0..4).map(peer).find(|p| !second.contains(p)).expect("two untried");
    announce.send(Heard { peer: outsider, txids: vec![txid] }).expect("watch running");
    validators[1].edit(|fake| {
        fake.listed.insert(txid);
        fake.bytes.insert(txid, transaction(2, 0).1);
    });
    fold().await;
    poll_all().await;
    let answered = answer.await.expect("submission task");
    assert_eq!(answered.expect("listed by a trusted validator"), txid);
    assert_eq!(validator_pushes(), 0, "no trusted validator saw it first, or at all from us");
    let spread = view.subscriber().current().spread(&txid).expect("held");
    assert!(spread.ours && spread.servable);
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 1, of: 4 }, Count { seen: 1, of: 2 })
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(peer_pushes().len(), 2, "spread past its entries: nothing more pushed");

    // nobody relays: the budget spent on peers, then one trusted verdict
    peers.pushes.lock().expect("fake peers mutex poisoned").clear();
    let (silent, sent) = transaction(3, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::time::sleep(Duration::from_secs(31)).await;
    let answered = answer.await.expect("submission task");
    assert_eq!(answered.expect("the verdict accepts"), silent);
    let tried = peer_pushes();
    assert_eq!(tried.len(), 3, "max_attempts peers first: {tried:?}");
    assert_eq!(tried.iter().map(|p| group(*p)).collect::<BTreeSet<_>>().len(), 3, "3 netgroups");
    assert_eq!(validator_pushes(), 1, "then exactly one trusted verdict");
    cancel.cancel();
}

/// Holding re-asked every poll (`getblockhash`, never a walk):
/// - verified tip ahead of the laggards → held by whoever has it, the rest `Behind`
/// - its only holder gone → no tip at all (fail closed: nothing proves the block valid)
/// - holders-only change → tip watch moves, epoch doesn't
/// - a heavier fork only one validator holds → it alone holds the tip, the others `Diverged`
/// - that one reorging away mid-poll → one wrong poll, dropped at the next
#[tokio::test]
async fn holders_are_reasked_every_poll_through_a_lost_holder_a_reorg_and_a_race() {
    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    let mut chain = Chain::new();
    let trunk_tip = chain.extend(chain.genesis().hash, 101);
    let trunk = chain.path(trunk_tip.hash);
    let at = |h: usize| BlockRef { hash: trunk[h].header().hash, height: trunk[h].header().height };
    for validator in &validators {
        validator.serve(&chain, at(100));
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
    let held_by = |positions: &[usize]| EndpointSet::at(positions.iter().copied());
    let agreements = || -> Vec<Agreement> {
        reader.current().endpoints().iter().map(|meta| meta.agreement).collect()
    };
    let tip = || reader.current().tip().map(|tip| (tip.block, tip.held_by));
    async fn open(tail: &mut crate::MempoolTail) -> bool {
        tokio::time::timeout(Duration::from_millis(20), tail.next()).await.is_err()
    }
    for poller in &pollers {
        poller.tick().await.expect("polls");
    }
    view.set_verified(verified(&chain, at(100)));
    assert_eq!(tip(), Some((at(100), held_by(&[0, 1, 2]))));
    let mut first = reader.tail().expect("a tip");

    // a mines 101; its header verifies: the tip moves at once, held by a alone
    validators[0].serve(&chain, at(101));
    pollers[0].tick().await.expect("a polls");
    view.set_verified(verified(&chain, at(101)));
    assert_eq!(tip(), Some((at(101), held_by(&[0]))), "one holder is enough");
    assert_eq!(agreements(), [Agreement::Agreed, Agreement::Behind, Agreement::Behind]);
    assert!(first.next().await.is_none(), "a new block ends the stream");
    let mut second = reader.tail().expect("a tip");

    // a goes unreachable: no trusted validator holds 101 → no tip (never a weaker answer)
    validators[0].chain.set_reachable(false);
    let failed = pollers[0].tick().await.expect_err("a unreachable");
    assert!(!pollers[0].failed(&failed, 1), "one failure = degraded, out of the holders");
    assert_eq!(tip(), None);
    let refused = reader.current().mempool().err();
    assert_eq!(refused, Some(Unserved::NotHeld { height: 101, configured: 3 }));
    assert_eq!(reader.current().best(), Some(at(101)), "the header chain still says 101");
    assert!(second.next().await.is_none(), "losing the tip ends the stream too");

    // b catches up: 101 served again; a returns: a holders-only change
    validators[1].serve(&chain, at(101));
    pollers[1].tick().await.expect("b polls");
    assert_eq!(tip(), Some((at(101), held_by(&[1]))));
    let mut third = reader.tail().expect("a tip");
    tips.borrow_and_update();
    validators[0].chain.set_reachable(true);
    pollers[0].tick().await.expect("a polls again");
    let joined = (*tips.borrow_and_update()).expect("a tip");
    assert_eq!((joined.block, joined.held_by), (at(101), held_by(&[0, 1])));
    assert!(open(&mut third).await, "holders-only change: same epoch, stream open");

    // a heavier fork from 100 that only c has: the tip follows the work, a and b diverge
    let fork = chain.mine(at(100).hash);
    validators[2].serve(&chain, fork);
    pollers[2].tick().await.expect("c polls");
    view.set_verified(verified(&chain, fork));
    assert_eq!(tip(), Some((fork, held_by(&[2]))));
    assert_eq!(agreements(), [Agreement::Diverged, Agreement::Diverged, Agreement::Agreed]);
    assert!(third.next().await.is_none(), "a reorg moves the tip block");

    // c raced: tip read on the fork, then back onto the trunk before its getblockhash answers:
    // one wrong poll (its claim still holds the fork), the next one re-asks and drops it
    validators[2].edit(|fake| fake.reorg_after_poll = Some(chain.path(at(101).hash)));
    let raced = pollers[2].tick().await.expect("a race is not a failure");
    assert_eq!(raced, Polled::Listed(0));
    assert_eq!(tip(), Some((fork, held_by(&[2]))), "the raced poll: its claim, as read");
    pollers[2].tick().await.expect("c polls again");
    assert_eq!(tip(), None, "re-asked: on the trunk, it holds nothing of the fork (never stale)");
    let pinned = reader.current();
    let c = &pinned.endpoints()[2];
    assert_eq!((c.tip(), c.agreement), (Some(at(101)), Agreement::Diverged));
    let links: usize = validators.iter().map(|v| v.read(|f| f.links_served)).sum();
    assert_eq!(links, 0, "holding is asked by getblockhash in the poll: no header reads");
}

/// Peers and release ride the poll every `METADATA_REFRESH`: a failed half keeps the endpoint live
/// and its last answer; a release halting within a week of the tip raises `ending`, an upgrade
/// clears it
#[tokio::test(start_paused = true)]
async fn metadata_rides_the_poll_and_a_failed_read_keeps_the_last_answer() {
    let mut chain = Chain::new();
    let tip = chain.extend(chain.genesis().hash, 10);
    let validator = Arc::new(FakeValidator::default());
    validator.serve(&chain, tip);
    let release = |build: &str, halts: u32| NodeRelease {
        build: build.to_owned(),
        user_agent: format!("/Zebra:{}/", &build[1..]),
        protocol_version: 170_140,
        end_of_service: EndOfService::At {
            height: Height::try_from(halts).expect("in range"),
            estimated_unix: 1_790_000_000,
        },
    };
    validator.edit(|fake| {
        fake.peers =
            vec![outbound("seed-a:8233"), PeerInfo { addr: "x:1".to_owned(), inbound: true }];
        fake.release = Some(release("v6.4.2", 10 + 4_960));
    });
    let (view, pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    let reader = view.subscriber();
    let peers =
        || -> Vec<PeerInfo> { reader.current().endpoints()[0].peers.iter().cloned().collect() };
    let build = || reader.current().endpoints()[0].release.as_ref().map(|r| r.build.clone());
    let one = EndpointSet::at([0]);

    pollers[0].tick().await.expect("first poll");
    let first = peers();
    assert_eq!(first.len(), 2);
    assert_eq!(build().as_deref(), Some("v6.4.2"));
    let pinned = reader.current();
    assert_eq!(pinned.endpoints()[0].blocks_to_end_of_service(), Some(4_960));
    assert_eq!(pinned.alarms().ending(), one, "halts within a week of its tip");

    validator.edit(|fake| {
        fake.peers_unreachable = true;
        fake.peers = Vec::new();
        fake.release = None;
    });
    tokio::time::advance(crate::config::METADATA_REFRESH).await;
    let polled = pollers[0].tick().await.expect("a metadata failure is not a poll failure");
    assert_eq!(polled, Polled::Listed(0));
    let pinned = reader.current();
    let meta = &pinned.endpoints()[0];
    assert_eq!((meta.state, meta.failures), (EndpointState::Live, 0));
    assert_eq!((peers(), build().as_deref()), (first, Some("v6.4.2")), "last answers kept");

    validator.edit(|fake| {
        fake.peers_unreachable = false;
        fake.release = Some(release("v6.5.0", 3_700_000));
    });
    pollers[0].tick().await.expect("between refreshes");
    assert_eq!(build().as_deref(), Some("v6.4.2"), "read once per refresh, not per tick");
    tokio::time::advance(crate::config::METADATA_REFRESH).await;
    pollers[0].tick().await.expect("next refresh");
    assert_eq!(peers(), [], "a fresh answer replaces it (isolated = empty, not an error)");
    assert_eq!(build().as_deref(), Some("v6.5.0"));
    assert_eq!(reader.current().alarms().ending(), EndpointSet::default(), "upgraded");
}

/// A validator that goes away is `Down` after the failure ceiling: its vote and sightings are
/// withdrawn (fail closed, never a stale vote) but its poller keeps running and retrying, and its
/// first answer back restores both; only cancel ends the poller
#[tokio::test(start_paused = true)]
async fn a_validator_that_goes_away_is_down_not_fatal_and_its_return_restores_its_hold() {
    let validator = Arc::new(FakeValidator::default());
    let tx1 = TransactionId::from([1u8; 32]);
    let chain = Chain::new();
    let genesis = chain.genesis();
    validator.serve(&chain, genesis);
    validator.edit(|fake| {
        fake.listed = [tx1].into_iter().collect();
        fake.bytes = [(tx1, vec![1u8; 8])].into_iter().collect();
    });
    let (view, mut pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    view.set_verified(verified(&chain, genesis));
    let reader = view.subscriber();
    let cancel = tokio_util::sync::CancellationToken::new();
    let polling = tokio::spawn(pollers.remove(0).run(cancel.clone()));
    let state = || reader.current().endpoints()[0].state;
    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..600 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("never {what}");
    }
    let serves_tx1 = || reader.current().mempool().is_ok_and(|mempool| mempool.get(&tx1).is_some());

    until("live", serves_tx1).await;

    validator.chain.set_reachable(false);
    until("down", || state() == EndpointState::Down).await;
    let pinned = reader.current();
    assert_eq!(pinned.tip(), None, "no holder left");
    let refused = pinned.mempool().err();
    assert_eq!(refused, Some(Unserved::NotHeld { height: 0, configured: 1 }), "fail closed");
    let trusted = pinned.spread(&tx1).map(|spread| spread.trusted);
    assert_eq!(trusted.unwrap_or_default(), Count::default(), "retracted, and none left reading");
    assert!(!polling.is_finished(), "a validator going away never ends its poller");

    validator.chain.set_reachable(true);
    until("back", serves_tx1).await;
    assert_eq!(state(), EndpointState::Live);

    cancel.cancel();
    polling.await.expect("only cancel ends the poller");
}

/// Unwoken = a poll a second; streaming = one reconcile per 15 s plus a poll per wake, a burst
/// of wakes coalesced to at most two polls; either streaming edge polls at once and shows in the
/// endpoint's metadata
#[tokio::test(start_paused = true)]
async fn a_push_stream_wakes_the_poller_and_stretches_its_reconcile_interval() {
    let validator = Arc::new(FakeValidator::default());
    let chain = Chain::new();
    validator.serve(&chain, chain.genesis());
    let (view, mut pollers) = ChainView::new(
        vec![Endpoint { address: "one:8232".to_owned(), source: Arc::clone(&validator) }],
        depth(),
    )
    .expect("one endpoint is a valid set");
    let reader = view.subscriber();
    let poller = pollers.remove(0);
    let waker = poller.waker();
    let cancel = tokio_util::sync::CancellationToken::new();
    let polling = tokio::spawn(poller.run(cancel.clone()));
    let polls = || validator.read(|f| f.polls);
    let streaming = || reader.current().endpoints()[0].streaming;
    let over = |millis: u64| tokio::time::sleep(Duration::from_millis(millis));

    over(10_100).await;
    assert_eq!(polls(), 11, "t = 0, then one a second");
    assert!(!streaming());

    waker.streaming(true);
    over(300).await;
    let edge = polls();
    assert_eq!(edge, 12, "stream up = poll at once (events may have fallen in the gap)");
    assert!(streaming());
    over(10_000).await;
    assert_eq!(polls(), edge, "streaming: nothing until a wake or the 15 s reconcile");

    for _ in 0..100 {
        waker.wake();
    }
    over(1_000).await;
    let burst = polls() - edge;
    assert!((1..=2).contains(&burst), "100 wakes = {burst} polls (one pending wake at most)");
    over(15_000).await;
    assert_eq!(polls(), edge + burst + 1, "the reconcile still runs while streaming");

    waker.streaming(false);
    over(300).await;
    assert!(!streaming(), "stream down = poll at once, back to the 1 s cadence");
    let down = polls();
    over(3_000).await;
    assert_eq!(polls(), down + 3);

    cancel.cancel();
    polling.await.expect("only cancel ends the poller");
}

/// Polls `done` every 20 ms for 10 s (header sync runs on its own task)
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never {what}");
}

/// Real regtest header bytes through header sync, three validators:
/// - a: 5,000 headers, verified in batches and finalized as they go (the published chain's final
///   tip = depth below its best), then its tip is the view's, held by a
/// - c: extends a's chain at 4,999 with a header earlier than its median time past: refused,
///   never moves the tip, holds nothing
/// - b: forks a's chain above the final boundary with more work: the tip follows the work to b
#[tokio::test]
async fn header_sync_verifies_every_validators_headers_and_the_tip_follows_the_work() {
    use zaino_header_chain::{HeaderChain, HeaderStore, Params};

    let mut chain = Chain::new();
    let a = chain.extend(chain.genesis().hash, 5_000);
    let trunk = chain.path(a.hash);
    let at = |h: usize| BlockRef { hash: trunk[h].header().hash, height: trunk[h].header().height };
    let early = chain.mine_at(at(4_998).hash, trunk[4_980].header().time);
    let c = chain.extend(early.hash, 11);
    let b = chain.extend(at(4_998).hash, 22);

    let validators: Vec<Arc<FakeValidator>> =
        (0..3).map(|_| Arc::new(FakeValidator::default())).collect();
    validators[0].serve(&chain, a);
    validators[1].serve(&chain, at(3_000));
    validators[2].serve(&chain, c);
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
    .expect("three endpoints");
    let reader = view.subscriber();
    let params = Params::regtest(Height::try_from(1).expect("1"), None).with_genesis(at(0).hash);
    let fs = zaino_persistence::fs::SimFs::new();
    let path = std::path::Path::new("/headers");
    let regtest = zcash_protocol::consensus::NetworkType::Regtest;
    let store = HeaderStore::open(fs, path, regtest).expect("store opens");
    let sync = view.header_sync(HeaderChain::open(params, depth(), store), validators.clone());
    let verified = sync.subscribe();
    let cancel = tokio_util::sync::CancellationToken::new();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    for poller in &pollers {
        poller.tick().await.expect("polls");
    }

    until("a's tip verified", || reader.current().best() == Some(a)).await;
    let tip = reader.current().tip().expect("a holds it");
    assert_eq!((tip.block, tip.held_by), (a, EndpointSet::at([0])), "c refused, b behind");
    let published = verified.borrow().clone().expect("a VerifiedChain");
    let answers = (published.best(), published.final_tip(), published.hash_at(at(2_500).height));
    let final_tip = Some(at(5_000 - 3));
    assert_eq!(answers, (a, final_tip, Some(at(2_500).hash)), "final = depth below the tip");
    assert!(!reader.current().alarms().finality_paused(), "held each batch: final as it went");

    validators[1].serve(&chain, b);
    pollers[1].tick().await.expect("b polls its fork");
    until("b's heavier fork verified", || reader.current().best() == Some(b)).await;
    let tip = reader.current().tip().expect("b holds it");
    assert_eq!((tip.block, tip.held_by), (b, EndpointSet::at([1])), "the work, not the first");
    assert_ne!(reader.current().best(), Some(c), "c's invalid chain never wins");
    until("b's chain published", || verified.borrow().as_ref().map(|v| v.best()) == Some(b)).await;
    assert_eq!(published.best(), a, "a published chain never changes (H5)");

    cancel.cancel();
    let ended = syncing.await.expect("header sync never panics");
    assert!(ended.is_ok(), "cancel ends header sync cleanly: {ended:?}");
}

/// One validator whose chain ends in an invalid 80 (time at its median time past), any work:
/// - first round: 0..=79 verified; its served run ends at the refused 80, off our chain, and no
///   poll has asked about our boundary yet: nothing final, the alarm raised
/// - next poll: `getblockhash` 76 / 79 = ours, so it holds the boundary: final through 76 (depth
///   3 below 79), the alarm cleared
/// - a valid fork above 79, the store failing its next commit: header sync ends with the error
#[tokio::test]
async fn finality_waits_only_for_a_trusted_holder_and_a_failed_commit_ends_header_sync() {
    use zaino_header_chain::{HeaderChain, HeaderStore, Params};

    let mut chain = Chain::new();
    let top = chain.extend(chain.genesis().hash, 79);
    let trunk = chain.path(top.hash);
    let at = |h: usize| BlockRef { hash: trunk[h].header().hash, height: trunk[h].header().height };
    let invalid = chain.mine_at(top.hash, trunk[60].header().time);
    let valid = chain.extend(top.hash, 5);
    let validator = Arc::new(FakeValidator::default());
    validator.serve(&chain, invalid);
    let endpoint = Endpoint { address: "a:8232".to_owned(), source: Arc::clone(&validator) };
    let (view, pollers) = ChainView::new(vec![endpoint], depth()).expect("one endpoint");
    let reader = view.subscriber();
    let params = Params::regtest(Height::try_from(1).expect("1"), None).with_genesis(at(0).hash);
    let fs = zaino_persistence::fs::SimFs::new();
    let path = std::path::Path::new("/headers");
    let regtest = zcash_protocol::consensus::NetworkType::Regtest;
    let store = HeaderStore::open(fs.clone(), path, regtest).expect("store opens");
    let sync = view.header_sync(HeaderChain::open(params, depth(), store), vec![validator.clone()]);
    let verified = sync.subscribe();
    let cancel = tokio_util::sync::CancellationToken::new();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

    pollers[0].tick().await.expect("polls");
    until("79 verified", || reader.current().best() == Some(at(79))).await;
    until("the alarm", || reader.current().alarms().finality_paused()).await;
    assert_eq!(published(), Some((at(79), None)), "no trusted holder of 76: nothing final");

    pollers[0].tick().await.expect("polls");
    until("the alarm cleared", || !reader.current().alarms().finality_paused()).await;
    assert_eq!(published(), Some((at(79), Some(at(76)))), "held: final to depth");

    fs.fail_from(fs.mutations());
    validator.serve(&chain, valid);
    pollers[0].tick().await.expect("polls");
    // the refused 80 stalls each round: the next one waits out `RETRY` (5 s)
    let ended = tokio::time::timeout(Duration::from_secs(30), syncing).await;
    let ended = ended.expect("ends on its own").expect("never panics");
    assert!(ended.is_err(), "a failed commit ends header sync: {ended:?}");
}
