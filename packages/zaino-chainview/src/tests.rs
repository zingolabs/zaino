//! Scenarios against `MockValidator`s (+ `MockPeers`), through the real balancer, on a paused clock
//!
//! - Each poll = the balancer's (every second, or woken); [`polled`] waits out one per member

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::HeaderViews;
use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{
    BlockRef, EndOfService, NodeRelease, PeerInfo, ReorgDepth, TransactionId, Zatoshis,
};
use zaino_source::testing::{raw_transaction, MockValidator, Port};
use zaino_source::{GetMempoolListingError, SendRawTransactionError};
use zaino_traffic::{Health, Limits, Push, TrafficBalancer, Trusted, ValidatorId};

use crate::testing::MockPeers;
use crate::{Agreement, ChainView, Count, EndpointSet, MempoolEntry, Projection, SubmitError};

/// The view over `validators` (`addresses` in order), its balancer + poll fold spawned (they run
/// once the test awaits)
fn running(
    validators: &[Arc<MockValidator>],
    addresses: &[&str],
) -> (ChainView<MockValidator>, TrafficBalancer<MockValidator>, CancellationToken) {
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = validators.iter().map(|validator| Trusted {
        source: Arc::clone(validator),
        priority: 0,
        limits,
    });
    let (balancer, driver) = TrafficBalancer::new(trusted.collect(), None);
    let addresses = addresses.iter().map(|address| (*address).to_owned()).collect();
    let view = ChainView::new(addresses, balancer.clone(), depth()).expect("a valid set");
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
        watch.changed().await.expect("driver running");
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

/// Mempool off below the network tip → `CatchingUp`: holds no tip (its tip may be stale), no
/// mempool, no chain description served; listings and the hold again once active
#[tokio::test(start_paused = true)]
async fn a_catching_up_validator_holds_no_tip_until_its_mempool_answers() {
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
    assert_eq!(held, (Some(at(11)), EndpointSet::default()), "catching up: never a holder");
    let meta = &catching_up.endpoints()[0];
    let answered = (meta.health, meta.tip(), meta.stale_blocks(), meta.agreement);
    let expected = (Health::CatchingUp, Some(at(11)), Some(29), Agreement::Agreed);
    assert_eq!(answered, expected, "an answer, no failure");
    assert_eq!(balancer.members().borrow().rows[0].failures, 0);
    let served = (catching_up.mempool().is_none(), catching_up.validator_info().is_none());
    assert_eq!(served, (true, true), "fail closed: no holder");
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
/// - announcements before any trusted listing: overheard, then join the sighting (`peers: 2/4,
///   trusted: 1/2`, first seen = the first announcement)
/// - submission → a peer first, never a trusted validator; black hole waited out, next entry in
///   another netgroup; outside announcer + trusted listing = the wallet's answer, no trusted push
/// - every peer a black hole (or dead): budget spent on peers, then one trusted verdict
#[tokio::test(start_paused = true)]
async fn peers_are_heard_first_entries_first_and_a_trusted_validator_gives_the_verdict() {
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
    tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
    tokio::task::yield_now().await; // the watch subscribes before the first announcement
    let fold = || tokio::time::sleep(crate::peers::PEER_FOLD * 2);
    let validator_pushes = || validators.iter().map(|v| v.calls().sends).sum::<usize>();
    polled(&balancer).await;

    // heard first: overheard until a trusted validator lists it
    let (gossiped, raw) = raw_transaction(1, 0);
    let first_heard = tokio::time::Instant::now();
    peers.announce(peer(1), vec![gossiped]);
    peers.announce(peer(2), vec![gossiped]);
    fold().await;
    assert_eq!(view.subscriber().current().spread(&gossiped), None, "peer-only = not held");
    tokio::time::sleep(Duration::from_secs(3)).await;
    validators[0].mempool_insert(raw, 1_000);
    polled(&balancer).await;
    let spread = view.subscriber().current().spread(&gossiped).expect("held once listed");
    assert_eq!(
        (spread.peers, spread.trusted),
        (Count { seen: 2, of: 4 }, Count { seen: 1, of: 2 })
    );
    assert_eq!(spread.timeline.first_seen, first_heard, "first seen = the first announcement");
    assert!(spread.timeline.first_trusted > Some(first_heard));
    assert_eq!(view.subscriber().current().peers_live.len(), 4);

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
    let group = crate::submit::Netgroup::of;
    assert_ne!(group(second[0]), group(second[1]), "another netgroup");
    let outsider = (0..4).map(peer).find(|p| !second.contains(p)).expect("two untried");
    peers.announce(outsider, vec![txid]);
    validators[1].mempool_insert(raw_transaction(2, 0).1, 1_000);
    fold().await;
    polled(&balancer).await;
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
    assert_eq!(peers.pushes().len(), 2, "spread past its entries: nothing more pushed");

    // nobody relays: the budget spent on peers, then one trusted verdict
    let (silent, sent) = raw_transaction(3, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::time::sleep(Duration::from_secs(31)).await;
    let answered = answer.await.expect("submission task");
    assert_eq!(answered.expect("the verdict accepts"), silent);
    let tried = peers.pushes().split_off(2);
    assert_eq!(tried.len(), 3, "max_attempts peers first: {tried:?}");
    assert_eq!(tried.iter().map(|p| group(*p)).collect::<BTreeSet<_>>().len(), 3, "3 netgroups");
    assert_eq!(validator_pushes(), 1, "then exactly one trusted verdict");
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
    c.changed().await.expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let raced = c.borrow().as_ref().map(|observation| observation.polled.is_ok());
    assert_eq!(raced, Some(true), "a race is not a failure");
    assert_eq!(tip(), Some((fork, held_by(&[2]))), "the raced poll: its claim, as read");
    c.changed().await.expect("driver running");
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

/// Real regtest header bytes through header sync, three validators
///
/// - a: 5,000 headers, verified in batches, finalized as they go (published final tip = depth
///   below its best); then its tip = the view's, held by a
/// - c: a's chain to 4,998 + a header at 4,999 earlier than its median time past → refused, never
///   the tip, holds nothing, reported (benched)
/// - b: fork of a's above the final boundary with more work → tip follows the work to b
#[tokio::test(start_paused = true)]
async fn header_sync_verifies_every_validators_headers_and_the_tip_follows_the_work() {
    use zaino_header_chain::{HeaderChain, HeaderStore};

    let mut chain = MockChain::regtest();
    let a = chain.mine_empty(5_000);
    let at = |height: u32| chain.at(h(height));
    let (at_2_500, at_3_000, at_4_990, at_4_998) = (at(2_500), at(3_000), at(4_990), at(4_998));
    let early = chain.block(at(4_980).hash).header().time;
    let c = chain.branch(at_4_998).mine_empty(12).tip();
    let b = chain.branch(at_4_998).mine_empty(22).tip();

    let validators = [a, at_3_000, c].map(|tip| Arc::new(MockValidator::following(&chain, tip)));
    validators[2].tamper(h(4_999), |header| header.time = early);
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"]);
    let reader = view.subscriber();
    let fs = zaino_persistence::fs::SimFs::new();
    let regtest = zcash_protocol::consensus::NetworkType::Regtest;
    let store = HeaderStore::open(fs, std::path::Path::new("/headers"), regtest).expect("opens");
    let sync = view.header_sync(HeaderChain::open(chain.header_params(), depth(), store));
    let verified = sync.subscribe();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));

    // the published chain, not the view's best: finality lands after the run reaches the view
    until("a's tip verified, final to depth", || published() == Some((a, Some(at_4_990)))).await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(a), EndpointSet::at([0])), "c refused, b behind");
    let first = verified.borrow().clone().expect("a VerifiedChain");
    assert_eq!(first.hash_at(at_2_500.height), Some(at_2_500.hash));
    assert!(!reader.current().alarms().finality_paused(), "held each batch: final as it went");
    let members = balancer.members().borrow().clone();
    let benched: Vec<bool> = members.rows.iter().map(|row| row.benched_until.is_some()).collect();
    assert_eq!(benched, [false, false, true], "c's invalid header reported");

    validators[1].follow(&chain, b);
    polled(&balancer).await;
    until("b's heavier fork verified", || reader.current().best() == Some(b)).await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(b), EndpointSet::at([1])), "the work, not the first");
    assert_ne!(reader.current().best(), Some(c), "c's invalid chain never wins");
    until("b's chain published", || published().map(|(best, _)| best) == Some(b)).await;
    assert_eq!(first.best(), a, "a published chain never changes (H5)");

    cancel.cancel();
    let ended = syncing.await.expect("header sync never panics");
    assert!(ended.is_ok(), "cancel ends header sync cleanly: {ended:?}");
}

/// First sync of 20,500 headers (> 10 batches) from one validator, a poll forced after every
/// publish (the production interleaving: each poll used to discard header sync's evidence)
///
/// - every published chain: final = best − depth (each run vouches itself, H6), best − final ≤
///   depth + `HEADER_BATCH` (H9), finality alarm down
#[tokio::test(start_paused = true)]
async fn a_first_sync_finalizes_every_batch_while_polls_interleave() {
    use zaino_header_chain::{HeaderChain, HeaderStore};

    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(20_500);
    let at_20_490 = chain.at(h(20_490));
    let validator = Arc::new(MockValidator::following(&chain, tip));
    let (view, balancer, cancel) = running(&[validator], &["a:8232"]);
    let reader = view.subscriber();
    let fs = zaino_persistence::fs::SimFs::new();
    let regtest = zcash_protocol::consensus::NetworkType::Regtest;
    let store = HeaderStore::open(fs, std::path::Path::new("/headers"), regtest).expect("opens");
    let sync = view.header_sync(HeaderChain::open(chain.header_params(), depth(), store));
    let mut verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));

    let mut publishes = 0;
    loop {
        let changed = tokio::time::timeout(Duration::from_secs(60), verified.changed()).await;
        changed.expect("header sync progresses").expect("header sync running");
        let (best, final_tip) = {
            let published = verified.borrow_and_update();
            let published = published.as_ref().expect("a chain once published");
            (published.best(), published.final_tip())
        };
        let base = final_tip.map_or(0, |tip| u32::from(tip.height) + 1);
        let unfinal = u32::from(best.height) + 1 - base;
        let bound = DEPTH + crate::headers::HEADER_BATCH;
        assert!(unfinal <= bound, "publish {publishes}: H9, {unfinal} unfinal > {bound}");
        let boundary = best.height.checked_sub(DEPTH);
        let final_height = final_tip.map(|tip| tip.height);
        assert_eq!(final_height, boundary, "publish {publishes}: final = best − depth (H6)");
        assert!(!reader.current().alarms().finality_paused(), "publish {publishes}: alarm");
        publishes += 1;
        balancer.pushed(v(0), Push::Changed);
        if (best, final_tip) == (tip, Some(at_20_490)) {
            break;
        }
    }
    assert!(publishes >= 10, "{publishes} publishes: one per batch at least");
    until("the view's tip, held", || reader.current().held_by() == EndpointSet::at([0])).await;
    cancel.cancel();
}

/// a's chain turns invalid at 31 (reported: benched), b honest; depth 10
///
/// - b's 30, then 31 followed within one poll while a backs off (a's stall is a's alone)
/// - a then serves a longer chain forked below our final tip: given up on that claim (never
///   fetched again while it stands), b's 32 followed within one poll
/// - store failing its next commit → header sync ends with the error at b's 33
#[tokio::test(start_paused = true)]
async fn a_benched_or_forked_validator_never_delays_the_honest_one() {
    use zaino_header_chain::{HeaderChain, HeaderStore};

    let mut chain = MockChain::regtest();
    let at_30 = chain.mine_empty(30);
    let (at_20, at_21, at_22) = (chain.at(h(20)), chain.at(h(21)), chain.at(h(22)));
    let early = chain.block(at_20.hash).header().time;
    let at_31 = chain.mine_empty(1);
    let forked = chain.fork(h(5)).mine_empty(60).tip();
    let validators = [at_31, at_30].map(|tip| Arc::new(MockValidator::following(&chain, tip)));
    validators[0].tamper(h(31), |header| header.time = early);
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232"]);
    let reader = view.subscriber();
    let fs = zaino_persistence::fs::SimFs::new();
    let path = std::path::Path::new("/headers");
    let regtest = zcash_protocol::consensus::NetworkType::Regtest;
    let store = HeaderStore::open(fs.clone(), path, regtest).expect("store opens");
    let sync = view.header_sync(HeaderChain::open(chain.header_params(), depth(), store));
    let verified = sync.subscribe();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.final_tip()));
    let followed = async |tip: BlockRef, final_tip: BlockRef| {
        let asked = tokio::time::Instant::now();
        until("followed", || published() == Some((tip, Some(final_tip)))).await;
        asked.elapsed()
    };

    followed(at_30, at_20).await;
    let benched = balancer.members().borrow().rows[0].benched_until.is_some();
    assert!(benched, "a's refused 31 reported");
    validators[1].follow(&chain, at_31);
    let took = followed(at_31, at_21).await;
    assert!(took <= Duration::from_millis(1_500), "b's 31 after {took:?}: a's stall delayed it");

    validators[0].follow(&chain, forked);
    tokio::time::sleep(Duration::from_secs(61)).await;
    let links = validators[0].calls().links;
    let at_32 = chain.branch(at_31).mine_empty(1).tip();
    validators[1].follow(&chain, at_32);
    let took = followed(at_32, at_22).await;
    assert!(took <= Duration::from_millis(1_500), "b's 32 after {took:?}");
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(validators[0].calls().links, links, "a's forked claim never fetched again");
    let pinned = reader.current();
    let standing = (pinned.best(), pinned.endpoints()[0].agreement);
    assert_eq!(standing, (Some(at_32), Agreement::Diverged), "below final: never followed");

    fs.fail_from(fs.mutations());
    let at_33 = chain.branch(at_32).mine_empty(1).tip();
    validators[1].follow(&chain, at_33);
    let ended = tokio::time::timeout(Duration::from_secs(10), syncing).await;
    let ended = ended.expect("ends on its own").expect("never panics");
    assert!(ended.is_err(), "a failed commit ends header sync: {ended:?}");
    cancel.cancel();
}
