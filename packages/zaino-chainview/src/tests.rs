//! Scenarios against `MockValidator`s (+ `MockPeers`), through the real balancer, on a paused clock
//!
//! - Each poll = the balancer's (every second, or woken); [`polled`] waits out one per member

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::HeaderViews;
use zaino_primitives::testing::{h, p2pkh, MockChain};
use zaino_primitives::types::{
    BlockRef, EndOfService, NodeRelease, OutPoint, PeerInfo, PeerTxId, ReorgDepth, TransactionId,
    Zatoshis,
};
use zaino_source::testing::{raw_transaction, raw_transparent, within, MockValidator, Port};
use zaino_source::{GetMempoolListingError, SendRawTransactionError};
use zaino_traffic::{Health, Limits, Push, TrafficBalancer, Trusted, ValidatorId};

use crate::listings::PEER_MEMPOOL_POLL;
use crate::testing::{MockPeers, MockUtxos};
use crate::{Agreement, ChainView, Count, EndpointSet, MempoolEntry, Projection, SubmitError};

/// The view over `validators` (`addresses` in order), its balancer + poll fold spawned (they run
/// once the test awaits)
fn running(
    validators: &[Arc<MockValidator>],
    addresses: &[&str],
) -> (ChainView<MockValidator>, TrafficBalancer<MockValidator>, CancellationToken) {
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = validators.iter().map(|validator| Trusted {
        source: Arc::clone(validator),
        priority: 0,
        limits,
    });
    let (balancer, driver) = TrafficBalancer::new(trusted.collect(), None);
    let addresses = addresses.iter().map(|address| (*address).to_owned()).collect();
    let view =
        ChainView::new(addresses, balancer.clone(), depth(), regtest()).expect("a valid set");
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_fold().run(cancel.clone()));
    (view, balancer, cancel)
}

/// Every member polled again (cadence, ladder or a wake) and that poll folded (paused clock: the
/// 1 ms timer fires once every task idles)
async fn polled(balancer: &TrafficBalancer<MockValidator>) {
    let members = balancer.members().borrow().rows.len();
    let mut watches: Vec<_> = (0..members).map(|at| balancer.observe(v(at))).collect();
    for watch in &mut watches {
        watch.borrow_and_update();
    }
    for watch in &mut watches {
        within("a member polled", watch.changed()).await.expect("driver running");
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
}

fn v(at: usize) -> ValidatorId {
    ValidatorId::new(at).expect("small")
}

fn outbound(addr: &str) -> PeerInfo {
    PeerInfo { addr: addr.to_owned(), inbound: false }
}

fn zats(zats: u64) -> Zatoshis {
    Zatoshis::new(zats).expect("in supply")
}

const DEPTH: u32 = 10;

fn depth() -> ReorgDepth {
    ReorgDepth::new(std::num::NonZeroU32::new(DEPTH).expect("nz"))
}

/// `MockChain::regtest()`'s: every upgrade through NU6.3 at 1
fn regtest() -> zaino_primitives::types::UpgradeSchedule {
    MockChain::regtest().schedule().upgrades
}

/// N=1, its window holding the verified tip
///
/// - mempool refused until a polled validator holds the verified best; then its listings, each
///   at its validator's fee
/// - `arrivals` since a pinned view = each crossing into servable once (a flap out and back,
///   or a drop: nothing); ours servable before any listing
/// - a mined block: the mempool as it now stands (ours listed)
#[tokio::test(start_paused = true)]
async fn a_single_endpoint_serves_its_listings_and_arrivals_are_each_crossing_into_servable() {
    let mut chain = MockChain::regtest();
    let tip_10 = chain.mine_empty(10);
    let validator = Arc::new(MockValidator::following(&chain, tip_10));
    let (one, one_raw) = raw_transaction(1, 0);
    let (two, two_raw) = raw_transaction(2, 0);
    let (three, three_raw) = raw_transaction(3, 0);
    validator.mempool_insert(one_raw.clone(), 1_000);
    validator.mempool_insert(two_raw.clone(), 2_000);

    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"]);
    let reader = view.subscriber();

    assert!(reader.current().mempool().is_none(), "no verified tip yet");
    view.set_verified(Some(chain.verified(tip_10)));
    let unheld = reader.current();
    assert_eq!((unheld.best(), unheld.mempool().is_none()), (Some(tip_10), true), "not polled");

    polled(&balancer).await;
    let pinned = reader.current();
    assert_eq!((pinned.best(), pinned.held_by()), (Some(tip_10), EndpointSet::at([0])));
    let entry = |txid: TransactionId, raw: &[u8], fee: u64| MempoolEntry {
        txid,
        raw: Bytes::from(raw.to_vec()),
        fee: Some(zats(fee)),
        projection: Projection::default(),
    };
    let entries: Vec<_> = pinned.mempool().expect("tip held").entries().collect();
    let mut listed = vec![entry(one, &one_raw, 1_000), entry(two, &two_raw, 2_000)];
    listed.sort_by_key(|entry| entry.txid);
    assert_eq!(entries, listed, "each entry: its validator's fee");

    // - tx 2 flaps out and back (servable in `pinned` → no arrival)
    // - tx 1 dropped (propagation churn: no arrival); tx 3 arrives; ours (tx 9) servable before
    //   any listing
    validator.mempool_remove(two);
    polled(&balancer).await;
    validator.mempool_remove(one);
    validator.mempool_insert(two_raw, 2_000);
    validator.mempool_insert(three_raw.clone(), 3_000);
    polled(&balancer).await;
    let (txid, sent) = raw_transaction(9, 0);
    let ours = view.submit(sent.clone()).await.expect("accepted");
    assert_eq!(ours, txid, "the txid from the bytes, not the validator's word");

    let raw = Bytes::from(sent);
    let projection = Projection::default();
    let unpriced = MempoolEntry { txid: ours, raw, fee: None, projection };
    let mut arrived = vec![entry(three, &three_raw, 3_000), unpriced];
    arrived.sort_by_key(|entry| entry.txid);
    assert_eq!(reader.current().arrivals(Some(&pinned)), arrived, "tx 3 + ours, tx 2 not again");
    assert_eq!(reader.current().arrivals(Some(&reader.current())), [], "nothing since itself");

    // block 11: the mempool as it now stands (ours listed)
    let tip_11 = chain.mine_empty(1);
    validator.follow(&chain, tip_11);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(tip_11)));
    let mined = reader.current();
    let now: Vec<_> = mined.mempool().expect("held").entries().map(|entry| entry.txid).collect();
    let mut current = vec![two, three, ours];
    current.sort();
    assert_eq!(now, current, "tx 1 gone, ours listed");
    cancel.cancel();
}

/// Mempool off below the network tip → `CatchingUp`: still holds the tip it claims (a trusted
/// validator's blocks are served whatever its sync state), no mempool served; listings again once
/// active
#[tokio::test(start_paused = true)]
async fn a_catching_up_validator_holds_its_tip_but_serves_no_mempool() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(12);
    let at = |height: u32| chain.at(h(height));
    let validator = Arc::new(MockValidator::following(&chain, at(10)));
    let (one, one_raw) = raw_transaction(1, 0);
    let (two, two_raw) = raw_transaction(2, 0);
    validator.mempool_insert(one_raw, 1_000);

    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"]);
    let reader = view.subscriber();

    polled(&balancer).await;
    assert_eq!(reader.current().endpoints()[0].health, Health::Live, "listed its mempool");
    view.set_verified(Some(chain.verified(at(10))));

    validator.follow(&chain, at(11));
    validator.estimate(h(40));
    validator.listing(Err(GetMempoolListingError::Inactive));
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at(11))));
    let catching_up = reader.current();
    let held = (catching_up.best(), catching_up.held_by());
    assert_eq!(held, (Some(at(11)), EndpointSet::at([0])), "catching up: still the holder");
    let meta = &catching_up.endpoints()[0];
    let answered = (meta.health, meta.tip(), meta.stale_blocks(), meta.agreement);
    let expected = (Health::CatchingUp, Some(at(11)), Some(29), Agreement::Agreed);
    assert_eq!(answered, expected, "an answer, no failure");
    assert_eq!(balancer.members().borrow().rows[0].failures, 0);
    let served = (catching_up.mempool().is_none(), catching_up.validator_info().is_some());
    assert_eq!(served, (true, true), "no mempool; its chain description served");
    assert_eq!(catching_up.spread(&one), None, "its only listing retracted: dropped");

    validator.follow(&chain, at(12));
    validator.estimate(h(12));
    validator.listing(Ok(()));
    validator.mempool_remove(one);
    validator.mempool_insert(two_raw, 2_000);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at(12))));
    let caught_up = reader.current();
    assert_eq!(caught_up.endpoints()[0].health, Health::Live);
    assert_eq!(
        caught_up.mempool().expect("held").entries().map(|e| e.txid).collect::<Vec<_>>(),
        [two],
    );
    cancel.cancel();
}

/// - Tip = the verified best block, never a validator's claim
/// - Holders = every validator whose chain holds it (one = enough)
/// - Third claiming a far higher tip: moves nothing, drops out of the holders, never supplies the
///   chain description served
/// - b, c unreachable at first (not yet read), each joining as it answers
#[tokio::test(start_paused = true)]
async fn a_claimed_higher_tip_moves_nothing_and_holders_are_whoever_holds_the_verified_block() {
    let mut chain = MockChain::regtest();
    let at_90 = chain.mine_empty(90);
    let agreed = chain.mine_empty(10);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(MockValidator::following(&chain, agreed))).collect();
    let (tx7, raw7) = raw_transaction(7, 0);
    for (validator, estimate) in validators.iter().zip([105u32, 106, 107]) {
        validator.estimate(h(estimate));
        validator.metadata(Some(vec![outbound("seed-a:8233")]), None);
    }
    let estimate = |pinned: &crate::ChainViewSnapshot| {
        pinned.validator_info().map(|info| u32::from(info.estimated_height))
    };
    validators[0].mempool_insert(raw7.clone(), 7_000);
    validators[1].reachable(&Port::ALL, false);
    validators[2].reachable(&Port::ALL, false);

    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"]);
    let reader = view.subscriber();
    view.set_verified(Some(chain.verified(agreed)));

    // one validator holding the verified block = a tip (one admission proves validity)
    polled(&balancer).await;
    let first = reader.current();
    assert_eq!((first.best(), first.held_by()), (Some(agreed), EndpointSet::at([0])));
    assert_eq!(estimate(&first), Some(105), "the holder's chain description");
    let spread = first.spread(&tx7).expect("endpoint a reported it");
    let one_of_one = Count { seen: 1, of: 1 };
    assert_eq!(spread.trusted, one_of_one, "b, c not yet read");
    assert!(spread.timeline.all_trusted.is_some(), "every reader so far lists it");
    let servable: Vec<_> = first.arrivals(None).iter().map(|entry| entry.txid).collect();
    assert_eq!(servable, [tx7], "servable once a holder lists it");

    validators[1].reachable(&Port::ALL, true);
    polled(&balancer).await;
    assert_eq!(reader.current().held_by(), EndpointSet::at([0, 1]), "a and b hold it");

    // c claims a higher tip (fork from 90, never verified): moves nothing, holds nothing
    let claimed = chain.branch(at_90).mine_empty(30).tip();
    validators[2].follow(&chain, claimed);
    validators[2].mempool_insert(raw7, 7_000);
    validators[2].metadata(Some(vec![outbound("seed-z:8233")]), None);
    validators[2].reachable(&Port::ALL, true);
    polled(&balancer).await;
    let pinned = reader.current();
    let held = (pinned.best(), pinned.held_by());
    assert_eq!(held, (Some(agreed), EndpointSet::at([0, 1])), "never the claim");
    assert_eq!(pinned.endpoints()[2].agreement, Agreement::Diverged, "holds neither 100 nor 97");
    assert_eq!(estimate(&pinned), Some(105), "never the outlier's");
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

    let again = pinned.arrivals(Some(&first));
    assert_eq!(again, [], "a second sighting spreads it, never arrives again");
    cancel.cancel();
}

/// One rejecting, one unreachable (sends only), one accepting validator
///
/// - random entries until one accepts, each pushed at most once
/// - accepted tx = `ours` (servable before any listing, unpriced)
/// - unanimous rejection = the rejection; none reachable = no answer; expired = refused, no push
#[tokio::test(start_paused = true)]
async fn a_submission_tries_random_entries_until_one_accepts_and_ours_is_servable_at_once() {
    let mut chain = MockChain::regtest();
    let agreed = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(MockValidator::following(&chain, agreed))).collect();
    let message = "tx unpaid action limit exceeded".to_owned();
    validators[1].relay(Err(SendRawTransactionError::Rejected { code: -26, message }));
    validators[2].reachable(&[Port::Send], false);

    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"]);
    let reader = view.subscriber();

    polled(&balancer).await;
    view.set_verified(Some(chain.verified(agreed)));
    assert!(!reader.current().held_by().is_empty(), "every one holds the verified tip");
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.calls().sends).collect() };

    let (expired_txid, expired) = raw_transaction(1, 50);
    let refused = view.submit(expired).await;
    let Err(SubmitError::Rejected(SendRawTransactionError::Rejected { code, message })) = refused
    else {
        panic!("expiry 50 at tip 50 = invalid in block 51: {refused:?}");
    };
    assert!(code == -25 && message.contains("expiry height 50"), "{code}: {message}");
    assert_eq!(pushes(), [0, 0, 0], "precheck refuses before any push");
    assert!(reader.current().spread(&expired_txid).is_none());

    let (txid, sent) = raw_transaction(9, 0);
    let accepted = view.submit(sent.clone()).await;
    assert_eq!(accepted.expect("one accept is enough; a refusal may be local policy"), txid);
    let tried = pushes();
    assert_eq!(tried[0], 1, "the accepting validator, once");
    assert!(tried.iter().all(|&n| n <= 1), "no validator pushed twice: {tried:?}");

    let pinned = reader.current();
    let spread = pinned.spread(&txid).expect("the submission recorded it");
    assert!(spread.ours && spread.servable);
    assert_eq!(spread.trusted, Count { seen: 0, of: 3 }, "not polled since");
    assert_eq!(spread.timeline.first_trusted, None);
    let servable = |view: &crate::ChainViewSnapshot| {
        view.mempool().expect("tip held").entries().find(|entry| entry.txid == txid)
    };
    let served = servable(&pinned).expect("`ours` is servable with zero sightings");
    let raw = Bytes::from(sent.clone());
    let unpriced = MempoolEntry { txid, raw, fee: None, projection: Projection::default() };
    assert_eq!(served, unpriced, "a wallet sees its own send before it propagates, unpriced");
    let render = |as_: &'static [u8]| move || Ok::<_, ()>(Bytes::from_static(as_));
    assert_eq!(
        served.projection.get_or_render(render(b"unpriced")),
        Ok(Bytes::from_static(b"unpriced"))
    );
    let again = servable(&reader.current()).expect("held");
    let cached = again.projection.get_or_render(render(b"rendered twice"));
    assert_eq!(cached, Ok(Bytes::from_static(b"unpriced")), "one render per (raw, fee)");

    // a lists what it accepted (fee = what its bytes leave: 0): the first listing prices it
    polled(&balancer).await;
    let pinned = reader.current();
    let listed = servable(&pinned).expect("still servable");
    let rerendered = listed.projection.get_or_render(render(b"priced"));
    assert_eq!(rerendered, Ok(Bytes::from_static(b"priced")), "priced → stale projection dropped");
    assert_eq!((listed.raw, listed.fee), (Bytes::from(sent), Some(zats(0))));
    let spread = pinned.spread(&txid).expect("held");
    assert_eq!(spread.trusted, Count { seen: 1, of: 3 });
    let timeline = spread.timeline;
    assert!(timeline.first_trusted.is_some_and(|at| at >= timeline.first_seen), "{timeline:?}");
    assert_eq!(timeline.all_trusted, None, "b, c have not listed it");

    // unanimous domain rejection = the real one, after every validator tried once
    for validator in &validators {
        let message = "too low fee".to_owned();
        validator.relay(Err(SendRawTransactionError::Rejected { code: -26, message }));
        validator.reachable(&[Port::Send], true);
    }
    let before = pushes();
    let rejected = view.submit(raw_transaction(8, 0).1).await;
    let too_low = SendRawTransactionError::Rejected { code: -26, message: "too low fee".into() };
    assert!(matches!(rejected, Err(SubmitError::Rejected(r)) if r == too_low), "the validator's");
    let tried: Vec<usize> = pushes().iter().zip(&before).map(|(now, then)| now - then).collect();
    assert_eq!(tried, [1, 1, 1]);

    // none reachable != a rejection (nothing learnt about the transaction)
    for validator in &validators {
        validator.reachable(&[Port::Send], false);
    }
    let sent = view.submit(raw_transaction(7, 0).1).await;
    assert!(matches!(sent, Err(SubmitError::Unreachable { attempted: 3, .. })), "{sent:?}");

    let garbage = view.submit(vec![9u8; 8]).await;
    assert!(matches!(garbage, Err(SubmitError::Rejected(SendRawTransactionError::Malformed(_)))));
    cancel.cancel();
}

/// Paused clock, every validator accepting, none gossiping on its own
///
/// - lone entry's listing ≠ spread → resubmitted to another after the threshold
/// - a never-entry validator lists it → job ends, nothing more pushed
#[tokio::test(start_paused = true)]
async fn an_unspread_submission_is_resubmitted_after_the_threshold_until_an_outsider_lists_it() {
    let chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(MockValidator::following(&chain, chain.genesis()))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"]);
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(4).expect("nz"),
    };
    let view = view.with_submit_policy(policy);
    polled(&balancer).await;
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.calls().sends).collect() };
    let (txid, sent) = raw_transaction(5, 0);
    let trusted = || view.subscriber().current().spread(&txid).expect("held").trusted;

    view.submit(sent.clone()).await.expect("the first entry accepts");
    let first = pushes();
    assert_eq!(first.iter().sum::<usize>(), 1);
    polled(&balancer).await;
    assert_eq!(trusted(), Count { seen: 1, of: 3 }, "the entry lists it");

    // ≤ 1 s polled + 8 s: inside the threshold
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(pushes(), first, "inside the threshold: no resubmit");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second = pushes();
    assert_eq!(second.iter().sum::<usize>(), 2, "threshold passed unspread: a second entry");
    assert!(second.iter().all(|&n| n <= 1), "a different validator: {second:?}");

    // the one validator never pushed to hears it by gossip and lists it: spread
    let outsider = second.iter().position(|&n| n == 0).expect("one untried");
    validators[outsider].mempool_insert(sent, 0);
    polled(&balancer).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(pushes(), second, "spread past its entries: the job ended, nothing more pushed");
    assert_eq!(trusted(), Count { seen: 3, of: 3 });
    cancel.cancel();
}

/// Paused clock, two trusted validators, four live peers in four netgroups + one dead
///
/// - each peer's mempool polled: `peers: x/4` = its listers; four netgroups = no quorum, so a
///   peer listing alone holds nothing (a trusted listing does: `peers: 2/4, trusted: 1/2`)
/// - submission → a peer first, never a trusted validator; black hole waited out, next entry in
///   another netgroup; outside lister + trusted listing = the wallet's answer, no trusted push
/// - every peer a black hole (or dead): budget spent on peers, then one trusted verdict
#[tokio::test(start_paused = true)]
async fn peers_list_first_entries_first_and_a_trusted_validator_gives_the_verdict() {
    let chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..2).map(|_| Arc::new(MockValidator::following(&chain, chain.genesis()))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232"]);
    let peer = |b: u8| std::net::SocketAddr::from(([10, b, 0, 1], 8233));
    let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    let polling = view.peer_mempool(Arc::new(MockUtxos::default())).expect("peers configured");
    tokio::spawn(polling.run(cancel.clone()));
    let round = || tokio::time::sleep(PEER_MEMPOOL_POLL);
    let id = |txid: TransactionId| PeerTxId { txid, auth_digest: None };
    let validator_pushes = || validators.iter().map(|v| v.calls().sends).sum::<usize>();
    polled(&balancer).await;

    // two peers list it: no quorum, not held until a trusted validator lists it
    let (gossiped, raw) = raw_transaction(1, 0);
    peers.list(peer(1), vec![id(gossiped)]);
    peers.list(peer(2), vec![id(gossiped)]);
    round().await;
    assert_eq!(view.subscriber().current().spread(&gossiped), None, "peer-listed only = not held");
    validators[0].mempool_insert(raw, 1_000);
    polled(&balancer).await;
    let spread = view.subscriber().current().spread(&gossiped).expect("held once listed");
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 2, of: 4 }, Count { seen: 1, of: 2 })
    );

    // submitted through peers: a black hole waited out, a relay answers the wallet
    let (txid, sent) = raw_transaction(2, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::task::yield_now().await;
    let first = peers.pushes();
    assert_eq!(
        (first.len(), validator_pushes()),
        (1, 0),
        "one peer entry, no validator: {first:?}"
    );
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(peers.pushes().len(), 1, "inside the threshold: no resubmit");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = peers.pushes();
    assert_eq!(second.len(), 2, "threshold passed unseen: a second peer");
    let group = crate::netgroup::Netgroup::of;
    assert_ne!(group(second[0]), group(second[1]), "another netgroup");
    let outsider = (0..4).map(peer).find(|p| !second.contains(p)).expect("two untried");
    peers.list(outsider, vec![id(txid)]);
    validators[1].mempool_insert(raw_transaction(2, 0).1, 1_000);
    round().await;
    polled(&balancer).await;
    let answered = within("the wallet answered", answer).await.expect("submission task");
    assert_eq!(answered.expect("listed by a trusted validator"), txid);
    assert_eq!(validator_pushes(), 0, "no trusted validator saw it first, or at all from us");
    let spread = view.subscriber().current().spread(&txid).expect("held");
    assert!(spread.ours && spread.servable);
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 1, of: 4 }, Count { seen: 1, of: 2 })
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(peers.pushes().len(), 2, "spread past its entries: nothing more pushed");

    // nobody relays: the budget spent on peers, then one trusted verdict
    let (silent, sent) = raw_transaction(3, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::time::sleep(Duration::from_secs(31)).await;
    let answered = within("the wallet answered", answer).await.expect("submission task");
    assert_eq!(answered.expect("the verdict accepts"), silent);
    let tried = peers.pushes().split_off(2);
    assert_eq!(tried.len(), 3, "max_attempts peers first: {tried:?}");
    assert_eq!(tried.iter().map(|p| group(*p)).collect::<BTreeSet<_>>().len(), 3, "3 netgroups");
    assert_eq!(validator_pushes(), 1, "then exactly one trusted verdict");
    cancel.cancel();
}

/// Paused clock, one trusted validator, seven live peers in seven netgroups, a UTXO set funding
/// `a` (10,000) and `bad` (5,000)
///
/// - `a` listed by the validator and every peer, then the validator down: still served, the same
///   sighting (handed over to the quorum, never dropped and re-added)
/// - every peer lists `a`, `bad` (pays 6,000 of 5,000) and `child` (spends `a`'s output): `a`
///   at its fee, `child` one round later (its parent admitted first), `bad` never (a negative fee:
///   refused once, its bytes never fetched again while it stays listed)
/// - `a` listed by 3 of 7 = kept (> 1/3), by 2 = gone from the view; the quorum still reads: an
///   empty mempool served
/// - a submission with no trusted validator: a peer entry, answered once 6 of 7 list it; no
///   longer listed = still served (ours) until the next block, then gone
#[tokio::test(start_paused = true)]
async fn with_every_trusted_validator_down_a_peer_quorum_keeps_the_mempool_and_takes_sends() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(5);
    let validator = Arc::new(MockValidator::following(&chain, tip));
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["a:8232"]);
    let peer = |b: u8| std::net::SocketAddr::from(([10, b, 0, 1], 8233));
    let seven: Vec<std::net::SocketAddr> = (1..=7).map(peer).collect();
    let peers = Arc::new(MockPeers::new(seven.clone(), []));
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    let alice = p2pkh([0xaa; 20]);
    let funding = |n: u8| OutPoint { txid: TransactionId::from([n; 32]), vout: 0 };
    let utxos = MockUtxos::of([(funding(1), zats(10_000)), (funding(2), zats(5_000))]);
    let polling = view.peer_mempool(Arc::new(utxos)).expect("peers configured");
    tokio::spawn(polling.run(cancel.clone()));
    let round = || tokio::time::sleep(PEER_MEMPOOL_POLL);
    let id = |txid: TransactionId| PeerTxId { txid, auth_digest: None };
    let reader = view.subscriber();
    let (a, a_raw) = raw_transparent(&[funding(1)], &[(&alice, 9_000)]);
    let (bad, bad_raw) = raw_transparent(&[funding(2)], &[(&alice, 6_000)]);
    let (child, child_raw) = raw_transparent(&[OutPoint { txid: a, vout: 0 }], &[(&alice, 8_500)]);
    for (txid, raw) in [(a, &a_raw), (bad, &bad_raw), (child, &child_raw)] {
        peers.hold(id(txid), Bytes::from(raw.clone()));
    }
    let listing = |ids: &[TransactionId], on: &[std::net::SocketAddr]| {
        for peer in &seven {
            let listed =
                if on.contains(peer) { ids.iter().copied().map(id).collect() } else { vec![] };
            peers.list(*peer, listed);
        }
    };
    let served = || -> Vec<(TransactionId, Option<Zatoshis>)> {
        let view = reader.current();
        let mempool = view.mempool().expect("a quorum reads");
        mempool.entries().map(|entry| (entry.txid, entry.fee)).collect()
    };
    let sorted = |mut entries: Vec<(TransactionId, Option<Zatoshis>)>| {
        entries.sort();
        entries
    };
    let first_seen =
        |txid: &TransactionId| reader.current().spread(txid).map(|s| s.timeline.first_seen);

    // listed by the validator and the quorum, then the validator gone: handed over, never dropped
    view.set_verified(Some(chain.verified(tip)));
    validator.mempool_insert(a_raw.clone(), 1_000);
    listing(&[a], &seven);
    polled(&balancer).await;
    round().await;
    let listed = first_seen(&a).expect("listed by the validator");
    assert_eq!(served(), [(a, Some(zats(1_000)))]);
    validator.reachable(&Port::ALL, false);
    // `Down` (its listing retracted) after 10 failures on the 0.5 → 30 s ladder
    tokio::time::sleep(Duration::from_secs(300)).await;
    let down = reader.current().endpoints()[0].health;
    assert_eq!(down, Health::Down, "its listing retracted, not kept as `Degraded`");
    assert!(reader.current().held_by().is_empty(), "no trusted validator vouches");
    assert_eq!(served(), [(a, Some(zats(1_000)))], "the quorum keeps it");
    assert_eq!(first_seen(&a), Some(listed), "the same sighting throughout");

    listing(&[a, bad, child], &seven);
    round().await;
    round().await;
    let both = sorted(vec![(a, Some(zats(1_000))), (child, Some(zats(500)))]);
    assert_eq!(sorted(served()), both, "funded ones at their fees; `bad` refused");
    round().await;
    round().await;
    let bad_fetches = peers.fetched().into_iter().filter(|fetched| fetched.txid == bad).count();
    assert_eq!(bad_fetches, 1, "refused once, never re-fetched while the quorum keeps listing it");
    let spread = reader.current().spread(&a).expect("held");
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 7, of: 7 }, Count { seen: 0, of: 0 })
    );

    listing(&[a, child], &seven[..3]);
    round().await;
    assert_eq!(sorted(served()), both, "3 of 7 still list them: kept");
    listing(&[a, child], &seven[..2]);
    round().await;
    assert_eq!(served(), [], "2 of 7: no longer kept; the quorum still reads, an empty mempool");
    assert_eq!((first_seen(&a), first_seen(&child)), (None, None), "gone from the view");

    // a send with no trusted validator: a peer first; the quorum listing it = the answer
    let (sent, sent_raw) = raw_transparent(&[funding(1)], &[(&alice, 9_500)]);
    peers.hold(id(sent), Bytes::from(sent_raw.clone()));
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent_raw).await });
    tokio::task::yield_now().await;
    let entry = peers.pushes();
    assert_eq!((entry.len(), validator.calls().sends), (1, 0), "one peer entry: {entry:?}");
    let others: Vec<std::net::SocketAddr> =
        seven.iter().copied().filter(|peer| *peer != entry[0]).collect();
    listing(&[sent], &others);
    round().await;
    let answered = within("the wallet answered", answer).await.expect("submission task");
    assert_eq!(answered.expect("a peer quorum admitted it"), sent);
    assert_eq!(served(), [(sent, Some(zats(500)))], "ours, priced from the UTXO set");

    // nobody lists it: ours keeps it served until the next block
    listing(&[], &seven);
    round().await;
    assert_eq!(served(), [(sent, Some(zats(500)))], "unlisted, ours: kept");
    let next = chain.mine_empty(1);
    view.set_verified(Some(chain.verified(next)));
    assert_eq!((served(), first_seen(&sent)), (vec![], None), "a block later: mined or gone");
    cancel.cancel();
}

/// Holding re-asked every poll (`getblockhash`, never a walk)
///
/// - verified tip ahead of the laggards → held by whoever has it, the rest `Behind`; slow mempool
///   bytes never delay the hold
/// - its only holder gone → no holder, no mempool (fail closed: nothing proves the block valid)
/// - a heavier fork only one validator holds → it alone holds the tip, the others `Diverged`
/// - that one reorging away mid-poll → one wrong poll, dropped at the next
#[tokio::test(start_paused = true)]
async fn holders_are_reasked_every_poll_through_a_lost_holder_a_reorg_and_a_race() {
    let mut chain = MockChain::regtest().varied_work();
    chain.mine_empty(101);
    let (at_100, at_101) = (chain.at(h(100)), chain.at(h(101)));
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(MockValidator::following(&chain, at_100))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"]);
    let reader = view.subscriber();
    let held_by = |positions: &[usize]| EndpointSet::at(positions.iter().copied());
    let agreements = || -> Vec<Agreement> {
        reader.current().endpoints().iter().map(|meta| meta.agreement).collect()
    };
    // best + its holders, `None` while nobody holds it
    let tip = || {
        let pinned = reader.current();
        let held = pinned.held_by();
        pinned.best().filter(|_| !held.is_empty()).map(|best| (best, held))
    };
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at_100)));
    assert_eq!(tip(), Some((at_100, held_by(&[0, 1, 2]))));

    // a mines 101, a new transaction's bytes 20 s away; its header verifies: the tip moves at
    // once, held by a alone (its poll's hold folded before its bytes are fetched)
    validators[0].follow(&chain, at_101);
    validators[0].latency(&[Port::MempoolBytes], Duration::from_secs(20));
    validators[0].mempool_insert(raw_transaction(1, 0).1, 1_000);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at_101)));
    assert_eq!(tip(), Some((at_101, held_by(&[0]))), "one holder is enough, bytes or not");
    assert_eq!(agreements(), [Agreement::Agreed, Agreement::Behind, Agreement::Behind]);
    validators[0].latency(&[Port::MempoolBytes], Duration::ZERO);
    tokio::time::sleep(Duration::from_secs(21)).await;

    // a goes unreachable: no trusted validator holds 101 → no mempool (never a weaker answer)
    validators[0].reachable(&Port::ALL, false);
    polled(&balancer).await;
    let health = reader.current().endpoints()[0].health;
    assert_eq!(health, Health::Degraded, "one failure = degraded, out of the holders");
    assert_eq!(tip(), None);
    assert!(reader.current().mempool().is_none(), "no holder: refused");
    assert_eq!(reader.current().best(), Some(at_101), "the header chain still says 101");

    // b catches up: 101 served again; a returns: a holders-only change
    validators[1].follow(&chain, at_101);
    polled(&balancer).await;
    assert_eq!(tip(), Some((at_101, held_by(&[1]))));
    validators[0].reachable(&Port::ALL, true);
    polled(&balancer).await;
    assert_eq!(tip(), Some((at_101, held_by(&[0, 1]))));

    // a heavier fork from 100 that only c has: the tip follows the work, a and b diverge
    let fork = chain.fork(h(100)).outweigh().mine_empty(1).tip();
    validators[2].follow(&chain, fork);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(fork)));
    assert_eq!(tip(), Some((fork, held_by(&[2]))));
    assert_eq!(agreements(), [Agreement::Diverged, Agreement::Diverged, Agreement::Agreed]);

    // c raced: tip read on the fork, then back onto the trunk before its getblockhash answers:
    // one wrong poll (its claim still holds the fork), the next one re-asks and drops it
    validators[2].reorg_after_next_poll(&chain, at_101);
    let mut c = balancer.observe(v(2));
    c.borrow_and_update();
    within("c polled", c.changed()).await.expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let raced = c.borrow().as_ref().map(|observation| observation.polled.is_ok());
    assert_eq!(raced, Some(true), "a race is not a failure");
    assert_eq!(tip(), Some((fork, held_by(&[2]))), "the raced poll: its claim, as read");
    within("c polled", c.changed()).await.expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(tip(), None, "re-asked: on the trunk, it holds nothing of the fork (never stale)");
    let pinned = reader.current();
    let c = &pinned.endpoints()[2];
    assert_eq!((c.tip(), c.agreement), (Some(at_101), Agreement::Diverged));
    let links: usize = validators.iter().map(|v| v.calls().links).sum();
    assert_eq!(links, 0, "holding is asked by getblockhash in the poll: no header reads");
    cancel.cancel();
}

/// Peers + release ride the poll every 60 s; the push stream's state, every poll
///
/// - failed half → endpoint still live, last answer kept
/// - release halting within a week of the tip raises `ending`; an upgrade clears it
/// - stream up / down (the balancer's) shown from the next poll
#[tokio::test(start_paused = true)]
async fn metadata_rides_the_poll_and_a_failed_read_keeps_the_last_answer() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(10);
    let validator = Arc::new(MockValidator::following(&chain, tip));
    let release = |build: &str, halts: u32| NodeRelease {
        build: build.to_owned(),
        user_agent: format!("/Zebra:{}/", &build[1..]),
        protocol_version: 170_140,
        end_of_service: EndOfService::At { height: h(halts), estimated_unix: 1_790_000_000 },
    };
    let inbound = PeerInfo { addr: "x:1".to_owned(), inbound: true };
    validator.metadata(
        Some(vec![outbound("seed-a:8233"), inbound]),
        Some(release("v6.4.2", 10 + 4_960)),
    );
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"]);
    let reader = view.subscriber();
    let peers =
        || -> Vec<PeerInfo> { reader.current().endpoints()[0].peers.iter().cloned().collect() };
    let build = || reader.current().endpoints()[0].release.as_ref().map(|r| r.build.clone());
    let streaming = || reader.current().endpoints()[0].streaming;
    let one = EndpointSet::at([0]);

    polled(&balancer).await;
    let first = peers();
    assert_eq!(first.len(), 2);
    assert_eq!(build().as_deref(), Some("v6.4.2"));
    let pinned = reader.current();
    assert_eq!(pinned.endpoints()[0].blocks_to_end_of_service(), Some(4_960));
    assert_eq!(pinned.alarms().ending(), one, "halts within a week of its tip");

    validator.metadata(None, None);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let health =
        (reader.current().endpoints()[0].health, balancer.members().borrow().rows[0].failures);
    assert_eq!(health, (Health::Live, 0), "a metadata failure is not a poll failure");
    assert_eq!((peers(), build().as_deref()), (first, Some("v6.4.2")), "last answers kept");

    validator.metadata(Some(Vec::new()), Some(release("v6.5.0", 3_700_000)));
    polled(&balancer).await;
    assert_eq!(build().as_deref(), Some("v6.4.2"), "read once per refresh, not per poll");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(peers(), [], "a fresh answer replaces it (isolated = empty, not an error)");
    assert_eq!(build().as_deref(), Some("v6.5.0"));
    assert_eq!(reader.current().alarms().ending(), EndpointSet::default(), "upgraded");

    assert!(!streaming());
    balancer.pushed(v(0), Push::Link(true));
    polled(&balancer).await;
    assert!(streaming(), "stream up, shown from the next poll");
    balancer.pushed(v(0), Push::Link(false));
    polled(&balancer).await;
    assert!(!streaming());
    cancel.cancel();
}

/// - Gone validator → `Down` past the failure ceiling (the balancer's): its holds + sightings
///   withdrawn (fail closed, never stale), still probed
/// - First answer back restores both
#[tokio::test(start_paused = true)]
async fn a_validator_that_goes_away_is_down_not_fatal_and_its_return_restores_its_hold() {
    let chain = MockChain::regtest();
    let genesis = chain.genesis();
    let validator = Arc::new(MockValidator::following(&chain, genesis));
    let (tx1, raw1) = raw_transaction(1, 0);
    validator.mempool_insert(raw1, 1_000);
    let (view, _, cancel) = running(&[Arc::clone(&validator)], &["one:8232"]);
    view.set_verified(Some(chain.verified(genesis)));
    let reader = view.subscriber();
    let state = || reader.current().endpoints()[0].health;
    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..600 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("never {what}");
    }
    let serves_tx1 = || {
        let pinned = reader.current();
        pinned.mempool().is_some_and(|mempool| mempool.entries().any(|entry| entry.txid == tx1))
    };

    until("live", serves_tx1).await;

    validator.reachable(&Port::ALL, false);
    until("down", || state() == Health::Down).await;
    let pinned = reader.current();
    assert_eq!(pinned.held_by(), EndpointSet::default(), "no holder left");
    assert!(pinned.mempool().is_none(), "fail closed");
    let trusted = pinned.spread(&tx1).map(|spread| spread.trusted);
    assert_eq!(trusted.unwrap_or_default(), Count::default(), "retracted, and none left reading");

    validator.reachable(&Port::ALL, true);
    until("back", serves_tx1).await;
    assert_eq!(state(), Health::Live);
    cancel.cancel();
}

/// Checks `done` every 20 ms (virtual) for 10 s, at idle points only (paused clock: every task
/// runs before a timer fires, blocking work holds the clock)
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never {what}");
}

/// Finality stall alarm (telemetry, depth 10): final = best − depth never raises it; a block
/// `depth` deep owed finality raises it only past 60 s; the final tip moving clears it
#[tokio::test(start_paused = true)]
async fn finality_stalled_rises_after_a_minute_unmoved_and_clears_when_final_moves() {
    let mut chain = MockChain::regtest();
    let tip_30 = chain.mine_empty(30);
    let tip_45 = chain.mine_empty(15);
    let validator = Arc::new(MockValidator::following(&chain, tip_45));
    let (view, _balancer, cancel) = running(&[validator], &["a:8232"]);
    let reader = view.subscriber();
    let stalled = || reader.current().alarms().finality_paused();

    view.set_verified(Some(chain.verified_final(tip_30, h(20))));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(!stalled(), "final = best − depth: nothing owed");
    view.set_verified(Some(chain.verified_final(tip_45, h(20))));
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert!(!stalled(), "unmoved 59 s: not yet");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(stalled(), "unmoved past 60 s, 25 unfinal");
    view.set_verified(Some(chain.verified_final(tip_45, h(35))));
    assert!(!stalled(), "final moved: cleared at once");
    cancel.cancel();
}

/// Real regtest header bytes through header sync, two trusted validators, depth 10
///
/// - a at 5,000: the chain anchors at its claim − max(depth, CONTEXT) (4,887), never reading
///   history below, then follows a's 113 headers; final = 4,990, held by a (b's claim 3,000 =
///   below final, held by definition, nothing fetched)
/// - b moves to a fork of a's at 4,999 with more work → the tip follows the work to b, held by b
/// - a published chain never changes; cancel ends header sync
#[tokio::test(start_paused = true)]
async fn header_sync_anchors_at_a_trusted_claim_and_the_tip_follows_the_work() {
    use zaino_header_chain::HeaderChain;

    let mut chain = MockChain::regtest();
    let a = chain.mine_empty(5_000);
    let at = |height: u32| chain.at(h(height));
    let (at_3_000, at_4_886, at_4_887) = (at(3_000), at(4_886), at(4_887));
    let (at_4_990, at_4_998) = (at(4_990), at(4_998));
    let b = chain.branch(at_4_998).mine_empty(22).tip();

    let validators = [a, at_3_000].map(|tip| Arc::new(MockValidator::following(&chain, tip)));
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232"]);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let verified = sync.subscribe();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

    until("anchored below a's claim, a's tip followed", || published() == Some((a, at_4_990)))
        .await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(a), EndpointSet::at([0])), "a holds the tip, b behind it");
    let first = verified.borrow().clone().expect("a VerifiedChain");
    let anchored = (first.hash_at(at_4_887.height), first.hash_at(at_4_886.height));
    assert_eq!(anchored, (Some(at_4_887.hash), None), "no history below the anchor");
    assert!(first.on_best(at_3_000), "b's claim: below final, final by definition");
    let fetched: Vec<usize> = validators.iter().map(|validator| validator.calls().links).collect();
    assert_eq!(fetched, [114, 0], "the anchor + 113 headers from a, nothing from b");
    assert!(!reader.current().alarms().finality_paused());

    validators[1].follow(&chain, b);
    polled(&balancer).await;
    until("b's heavier fork followed", || reader.current().best() == Some(b)).await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(b), EndpointSet::at([1])), "the work, not the first");
    until("b's chain published", || published().map(|(best, _)| best) == Some(b)).await;
    assert_eq!(first.best(), a, "a published chain never changes (H5)");

    cancel.cancel();
    syncing.await.expect("header sync never panics");
}

/// A validator far ahead, a poll forced after every publish: the chain re-anchors at its claim
/// − depth instead of syncing headers toward it
///
/// - a at 20: anchored at genesis (20 − max(depth, CONTEXT), saturated), followed to 20, final 10
/// - a jumps to 20,500 (past the ceiling: depth + `HEADER_BATCH` above final) → re-anchored at
///   20,387, followed to 20,500, final 20,490: 135 headers fetched in all, none of the 20,366
///   between
/// - every published chain: best − final ≤ depth + `HEADER_BATCH` (H9), finality alarm down; the
///   view's tip held by a
#[tokio::test(start_paused = true)]
async fn a_validator_far_ahead_re_anchors_the_chain_instead_of_syncing_toward_it() {
    use zaino_header_chain::HeaderChain;

    let mut chain = MockChain::regtest();
    let at_20 = chain.mine_empty(20);
    let tip = chain.mine_empty(20_480);
    let (genesis, at_10) = (chain.genesis(), chain.at(h(10)));
    let (at_20_387, at_20_490) = (chain.at(h(20_387)), chain.at(h(20_490)));
    let validator = Arc::new(MockValidator::following(&chain, at_20));
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["a:8232"]);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let mut verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));

    let mut followed = Vec::new();
    loop {
        let changed = tokio::time::timeout(Duration::from_secs(60), verified.changed()).await;
        changed.expect("header sync progresses").expect("header sync running");
        let (best, final_tip) = {
            let published = verified.borrow_and_update();
            let published = published.as_ref().expect("a chain once published");
            (published.best(), published.final_tip())
        };
        let unfinal = u32::from(best.height) - u32::from(final_tip.height);
        let bound = DEPTH + crate::headers::HEADER_BATCH;
        assert!(unfinal <= bound, "{best:?}: H9, {unfinal} unfinal > {bound}");
        assert!(!reader.current().alarms().finality_paused(), "{best:?}: alarm");
        followed.push((best, final_tip));
        balancer.pushed(v(0), Push::Changed);
        if (best, final_tip) == (at_20, at_10) {
            validator.follow(&chain, tip);
        }
        if (best, final_tip) == (tip, at_20_490) {
            break;
        }
    }
    let finals: Vec<BlockRef> = followed.iter().map(|(_, final_tip)| *final_tip).collect();
    let anchors_and_finals = [genesis, at_10, at_20_387, at_20_490];
    let jumped = finals.iter().all(|final_tip| anchors_and_finals.contains(final_tip));
    assert!(jumped, "final only ever the two anchors or their runs' finals: {finals:?}");
    let links = validator.calls().links;
    assert_eq!(links, 1 + 20 + 1 + 113, "two anchors + their runs, nothing between");
    until("the view's tip, held", || reader.current().held_by() == EndpointSet::at([0])).await;
    cancel.cancel();
}

/// Two trusted validators at 30, depth 10; a then serves a longer chain forked at 5, below our
/// final tip
///
/// - a's forked claim: given up on (never fetched again while it stands), `Diverged`, never the
///   tip (no re-anchor onto a chain leaving ours below final)
/// - b's 31 and 32 followed within one poll each meanwhile (a's standing is a's alone)
#[tokio::test(start_paused = true)]
async fn a_forked_validator_never_delays_the_honest_one() {
    use zaino_header_chain::HeaderChain;

    let mut chain = MockChain::regtest();
    let at_30 = chain.mine_empty(30);
    let at_31 = chain.mine_empty(1);
    let at_32 = chain.mine_empty(1);
    let (at_20, at_21, at_22) = (chain.at(h(20)), chain.at(h(21)), chain.at(h(22)));
    let forked = chain.fork(h(5)).mine_empty(60).tip();
    let validators = [at_30, at_30].map(|tip| Arc::new(MockValidator::following(&chain, tip)));
    let (view, _balancer, cancel) = running(&validators, &["a:8232", "b:8232"]);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));
    let followed = async |tip: BlockRef, final_tip: BlockRef| {
        let asked = tokio::time::Instant::now();
        until("followed", || published() == Some((tip, final_tip))).await;
        asked.elapsed()
    };

    followed(at_30, at_20).await;
    validators[0].follow(&chain, forked);
    validators[1].follow(&chain, at_31);
    let took = followed(at_31, at_21).await;
    assert!(took <= Duration::from_millis(1_500), "b's 31 after {took:?}");

    tokio::time::sleep(Duration::from_secs(61)).await;
    let links = validators[0].calls().links;
    validators[1].follow(&chain, at_32);
    let took = followed(at_32, at_22).await;
    assert!(took <= Duration::from_millis(1_500), "b's 32 after {took:?}");
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(validators[0].calls().links, links, "a's forked claim never fetched again");
    let pinned = reader.current();
    let standing = (pinned.best(), pinned.endpoints()[0].agreement);
    assert_eq!(standing, (Some(at_32), Agreement::Diverged), "below final: never followed");
    cancel.cancel();
}

/// The peer set over one `MockChain`'s best path up to `held`: headers after the first locator
/// hash it holds (zebra's `FindHeaders`: up to 160), blocks by hash
struct PeerSet {
    best: Vec<BlockRef>,
    held: std::sync::Mutex<usize>,
    headers: std::collections::HashMap<zaino_primitives::types::BlockHash, Vec<u8>>,
    blocks: Vec<Arc<zaino_primitives::types::Block>>,
}

impl zaino_traffic::PeerTransport for PeerSet {
    fn headers(
        &self,
        locator: Vec<zaino_primitives::types::BlockHash>,
        _: Option<zaino_primitives::types::BlockHash>,
    ) -> futures::future::BoxFuture<'static, Result<Vec<Vec<u8>>, zaino_source::NonDomainError>>
    {
        let held = &self.best[..=*self.held.lock().expect("peer set mutex")];
        let known = locator.iter().find_map(|hash| held.iter().position(|b| b.hash == *hash));
        let after = known.map_or(&[][..], |at| &held[at + 1..]);
        let headers = after.iter().take(160).map(|block| self.headers[&block.hash].clone());
        Box::pin(std::future::ready(Ok(headers.collect())))
    }

    fn block(
        &self,
        hash: zaino_primitives::types::BlockHash,
    ) -> futures::future::BoxFuture<
        'static,
        Result<zaino_primitives::types::Block, zaino_source::NonDomainError>,
    > {
        let block = self.blocks.iter().find(|block| block.header().hash == hash);
        let block =
            block.map(|block| zaino_primitives::types::Block::clone(block)).ok_or_else(|| {
                zaino_source::NonDomainError::new(zaino_source::FailureMode::Connection, "absent")
            });
        Box::pin(std::future::ready(block))
    }
}

/// One trusted validator + the peer set, both at 40, depth 10, real regtest headers:
/// - a anchors and leads to 40, final 30
/// - a down, the network moves on to 60: the peer set's 41..=60 each pass every rule; best = 60,
///   never vouched, so final stops at 40 = a's last vouch (best − depth would be 50), best held
///   by no one
/// - a back at 60: it vouches the peers' chain, final = 50, held by a
#[tokio::test(start_paused = true)]
async fn peers_extend_the_tip_past_a_down_validator_and_finality_waits_for_its_return() {
    use zaino_header_chain::{HeaderChain, Params};
    use zaino_traffic::Peers;

    let mut chain = MockChain::regtest();
    let at_40 = chain.mine_empty(40);
    let at_60 = chain.mine_empty(20);
    let (at_30, at_50) = (chain.at(h(30)), chain.at(h(50)));
    let best: Vec<BlockRef> = (0..=60).map(|height| chain.at(h(height))).collect();
    let headers = best.iter().map(|block| (block.hash, chain.header_bytes(block.hash))).collect();
    let held = std::sync::Mutex::new(40);
    let peer_set = Arc::new(PeerSet { best, held, headers, blocks: chain.blocks(at_60) });
    let validator = Arc::new(MockValidator::following(&chain, at_40));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&validator), priority: 0, limits }];
    let peers = Peers { transport: Arc::clone(&peer_set) as _, in_flight: 4 };
    let (balancer, driver) = TrafficBalancer::new(trusted, Some(peers));
    let view = ChainView::new(vec!["a:8232".to_owned()], balancer.clone(), depth(), regtest());
    let view = view.expect("a valid set");
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_fold().run(cancel.clone()));
    let reader = view.subscriber();
    let rules = Params::regtest(h(1), None);
    let sync = view.header_sync(HeaderChain::new(depth())).with_peers(rules);
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

    until("anchored, a followed", || published() == Some((at_40, at_30))).await;

    validator.reachable(&Port::ALL, false);
    *peer_set.held.lock().expect("peer set mutex") = 60;
    until("the peers' tip, unvouched", || published() == Some((at_60, at_40))).await;
    until("best held by no one", || reader.current().held_by() == EndpointSet::default()).await;
    assert_eq!(reader.current().best(), Some(at_60));

    validator.follow(&chain, at_60);
    validator.reachable(&Port::ALL, true);
    until("a vouches the peers' chain", || published() == Some((at_60, at_50))).await;
    until("held by a", || reader.current().held_by() == EndpointSet::at([0])).await;
    cancel.cancel();
}

/// a at 40 (final 40 once best ≥ 50), the peer set's best path above it; each row = what the
/// peers' path ends in, where the tip stops, whether the peers were blamed:
/// - 2,100 good headers: the ceiling (final + depth + `HEADER_BATCH` = 2,050) exactly
/// - block 45 from the future: 44, no blame (only the clock is behind)
/// - block 45 with nBits the rules refuse: 44, each refusal a misanswer on the peer set
#[tokio::test(start_paused = true)]
async fn peer_headers_stop_at_the_ceiling_wait_out_the_clock_and_a_broken_rule_is_a_misanswer() {
    use metrics_util::debugging::DebuggingRecorder;
    use zaino_header_chain::{HeaderChain, Params};
    use zaino_traffic::Peers;

    let blamed_peers = |key: &metrics_util::CompositeKey| {
        key.key().name() == "zaino_traffic_misanswers_total"
            && key.key().labels().any(|l| l.key() == "member" && l.value() == "peers")
    };

    type PathEnd = fn(&mut MockChain) -> BlockRef;
    let rows: [(&str, PathEnd, (u32, u32), bool); 3] = [
        ("past the ceiling", |chain| chain.mine_empty(2_060), (2_050, 40), false),
        ("from the future", |chain| chain.mine(|b| b.time(4_000_000_000)), (44, 34), false),
        ("broken nBits", |chain| chain.mine(|b| b.bits(0x1d00_ffff)), (44, 34), true),
    ];
    for (row, path_end, (tip, final_tip), blamed) in rows {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recording = metrics::set_default_local_recorder(&recorder);
        let mut chain = MockChain::regtest().varied_work();
        let at_40 = chain.mine_empty(40);
        let validator = Arc::new(MockValidator::following(&chain, at_40));
        chain.mine_empty(4);
        path_end(&mut chain);
        let peers_tip = chain.mine_empty(1);
        let blocks = chain.blocks(peers_tip);
        let best: Vec<BlockRef> = blocks.iter().map(|block| block.at()).collect();
        let headers = best.iter().map(|at| (at.hash, chain.header_bytes(at.hash))).collect();
        let held = std::sync::Mutex::new(best.len() - 1);
        let peer_set = Arc::new(PeerSet { best: best.clone(), held, headers, blocks });
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = vec![Trusted { source: Arc::clone(&validator), priority: 0, limits }];
        let peers = Peers { transport: peer_set as _, in_flight: 4 };
        let (balancer, driver) = TrafficBalancer::new(trusted, Some(peers));
        let view = ChainView::new(vec!["a:8232".to_owned()], balancer.clone(), depth(), regtest());
        let view = view.expect("a valid set");
        let cancel = CancellationToken::new();
        tokio::spawn(driver.run(cancel.clone()));
        tokio::spawn(view.observation_fold().run(cancel.clone()));
        let sync =
            view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(h(1), None));
        let verified = sync.subscribe();
        tokio::spawn(sync.run(cancel.clone()));
        let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

        let expected = Some((best[tip as usize], best[final_tip as usize]));
        until(row, || published() == expected).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(published(), expected, "{row}: stays (six peer polls later)");
        let recorded = snapshotter.snapshot().into_vec();
        let peers_blamed = recorded.iter().any(|(key, ..)| blamed_peers(key));
        assert_eq!(peers_blamed, blamed, "{row}: peers blamed");
        cancel.cancel();
    }
}

/// The previous scenario's outage (a down at 40, peers on to 60, depth 10), now with peers' block
/// `inv`s: final reaches best − depth = 50 on the peers' word once more than 5 netgroups carry it
/// - 10.1–10.5 (two in 10.5) announce 60: five netgroups, final stays 40
/// - 10.7 announces a block off our chain; 10.8 (not live: inbound) announces 60; 10.6 announces
///   45 (ours, but below 50: its chain may leave ours after 45): none counts
/// - 10.6 then announces 55 (ours, above 50): six netgroups → final 50, a still down
#[tokio::test(start_paused = true)]
async fn six_netgroups_carrying_the_chain_finalize_it_while_no_trusted_validator_vouches() {
    use zaino_header_chain::{HeaderChain, Params};
    use zaino_traffic::Peers;

    let mut chain = MockChain::regtest();
    let at_40 = chain.mine_empty(40);
    let at_60 = chain.mine_empty(20);
    let (at_30, at_45) = (chain.at(h(30)), chain.at(h(45)));
    let (at_50, at_55) = (chain.at(h(50)), chain.at(h(55)));
    let elsewhere = chain.fork(h(45)).mine_empty(3).tip();
    let best: Vec<BlockRef> = (0..=60).map(|height| chain.at(h(height))).collect();
    let headers = best.iter().map(|block| (block.hash, chain.header_bytes(block.hash))).collect();
    let held = std::sync::Mutex::new(40);
    let peer_set = Arc::new(PeerSet { best, held, headers, blocks: chain.blocks(at_60) });
    let validator = Arc::new(MockValidator::following(&chain, at_40));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&validator), priority: 0, limits }];
    let peers = Peers { transport: Arc::clone(&peer_set) as _, in_flight: 4 };
    let (balancer, driver) = TrafficBalancer::new(trusted, Some(peers));
    let peer = |b: u8, host: u8| std::net::SocketAddr::from(([10, b, 0, host], 8233));
    let live = [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2), (6, 1), (7, 1)];
    let announcing = Arc::new(MockPeers::new(live.map(|(b, host)| peer(b, host)), []));
    let view = ChainView::new(vec!["a:8232".to_owned()], balancer.clone(), depth(), regtest());
    let view = view.expect("a valid set").with_peers(announcing.clone());
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_fold().run(cancel.clone()));
    tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
    let sync = view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(h(1), None));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

    until("anchored, a followed", || published() == Some((at_40, at_30))).await;
    validator.reachable(&Port::ALL, false);
    *peer_set.held.lock().expect("peer set mutex") = 60;
    until("the peers' tip, unvouched", || published() == Some((at_60, at_40))).await;

    for (b, host) in [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2)] {
        announcing.announce_block(peer(b, host), at_60.hash);
    }
    announcing.announce_block(peer(7, 1), elsewhere.hash);
    announcing.announce_block(peer(8, 1), at_60.hash);
    announcing.announce_block(peer(6, 1), at_45.hash);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(published(), Some((at_60, at_40)), "five netgroups: final unmoved");

    announcing.announce_block(peer(6, 1), at_55.hash);
    until("final on six netgroups' word", || published() == Some((at_60, at_50))).await;
    let held_by = view.subscriber().current().held_by();
    assert_eq!(held_by, EndpointSet::default(), "a still down: vouched nothing");
    cancel.cancel();
}
