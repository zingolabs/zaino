//! Scenarios against `MockValidator`s (+ `MockPeers`), through the real balancer, on a paused clock
//!
//! - Each poll = the balancer's (every second, or woken); [`polled`] waits out one per member

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::HeaderViews;
use zaino_primitives::testing::{h, p2pkh, MockChain};
use zaino_primitives::types::{BlockRef, FinalityDepth, PeerTxId};
use zaino_source::testing::{
    decoded, raw_transaction, raw_transparent, within, MockValidator, Port, ValidatorViews,
};
use zaino_source::{
    EndOfService, GetMempoolListingError, NodeRelease, PeerInfo, SendRawTransactionError,
};
use zaino_traffic::testing::MockPeerSet;
use zaino_traffic::{
    Health, Limits, PeerTransport, Peers, Push, TrafficBalancer, TrustedValidator,
    TrustedValidatorId,
};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::bundle::OutPoint;

use crate::listings::PEER_MEMPOOL_POLL;
use crate::testing::MockPeers;
use crate::{Agreement, ChainView, MempoolEntry, Projection, SubmitError, TrustedValidatorSet};

/// The view over `validators` (`addresses` in order) + `peer_set`, its balancer + poll merge
/// spawned (they run once the test awaits)
fn running(
    validators: &[Arc<MockValidator>],
    addresses: &[&str],
    peer_set: Option<Arc<dyn PeerTransport>>,
) -> (ChainView<MockValidator>, TrafficBalancer<MockValidator>, CancellationToken) {
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = validators.iter().map(|validator| TrustedValidator {
        source: Arc::clone(validator),
        priority: 0,
        limits,
    });
    let peers = peer_set.map(|transport| Peers { transport, in_flight: 4 });
    let (balancer, driver) = TrafficBalancer::new(trusted.collect(), peers);
    let addresses = addresses.iter().map(|address| (*address).to_owned()).collect();
    let view = ChainView::new(
        addresses,
        balancer.clone(),
        depth(),
        regtest(),
        tokio_util::sync::CancellationToken::new(),
    )
    .expect("a valid set");
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_merge().run(cancel.clone()));
    (view, balancer, cancel)
}

/// Every member polled again (cadence, ladder or a wake) and that poll merged (paused clock: the
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

fn v(at: usize) -> TrustedValidatorId {
    TrustedValidatorId::new(at).expect("small")
}

fn outbound(addr: &str) -> PeerInfo {
    PeerInfo { addr: addr.to_owned(), inbound: false }
}

fn zats(zats: u64) -> Zatoshis {
    Zatoshis::from_u64(zats).expect("in supply")
}

const DEPTH: u32 = 10;

fn depth() -> FinalityDepth {
    FinalityDepth::new(std::num::NonZeroU32::new(DEPTH).expect("nz"))
}

/// `MockChain::regtest()`'s: every upgrade through NU6.3 at 1
fn regtest() -> zcash_protocol::local_consensus::LocalNetwork {
    MockChain::regtest().schedule().upgrades
}

/// N=1, its window holding the verified tip; coinbases at 1..=3 fund txs 1..=3 (fee n · 1 000)
///
/// - mempool refused until a polled validator holds the verified best; then its listings, each
///   at its validator's fee
/// - `arrivals` since a pinned view = each transaction held now and absent then, once (a flap
///   out and back, or a drop: nothing); ours served before any listing
/// - a mined block: the mempool as it now stands (ours listed)
#[tokio::test(start_paused = true)]
async fn a_single_trusted_validator_serves_its_listings_and_arrivals_are_each_new_transaction_once()
{
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    for n in 1..=3u8 {
        chain.mine(|b| b.coinbase(|c| c.txid([n; 32]).pay(&alice, 10_000)));
    }
    let tip_10 = chain.mine_empty(7);
    let [(one, one_raw), (two, two_raw), (three, three_raw)] = [1u8, 2, 3].map(|n| {
        let fee = u64::from(n) * 1_000;
        raw_transparent(&[OutPoint::new([n; 32], 0)], &[(&alice, 10_000 - fee)])
    });
    for raw in [&one_raw, &two_raw, &three_raw] {
        chain.submit(decoded(raw.clone()));
    }
    let validator = Arc::new(chain.validator(tip_10));
    validator.hear(&chain, one);
    validator.hear(&chain, two);

    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"], None);
    let reader = view.subscriber();

    assert!(reader.current().mempool().is_none(), "no verified tip yet");
    view.set_verified(Some(chain.verified(tip_10)));
    let unheld = reader.current();
    assert_eq!((unheld.best(), unheld.mempool().is_none()), (Some(tip_10), true), "not polled");

    polled(&balancer).await;
    let pinned = reader.current();
    assert_eq!((pinned.best(), pinned.held_by()), (Some(tip_10), TrustedValidatorSet::at([0])));
    let entry = |txid: TxId, raw: &[u8], fee: u64| MempoolEntry {
        txid,
        raw: Bytes::from(raw.to_vec()),
        fee: Some(zats(fee)),
        projection: Projection::default(),
    };
    let entries: Vec<_> = pinned.mempool().expect("tip held").entries().collect();
    let mut listed = vec![entry(one, &one_raw, 1_000), entry(two, &two_raw, 2_000)];
    listed.sort_by_key(|entry| entry.txid);
    assert_eq!(entries, listed, "each entry: its validator's fee");

    // - tx 2 flaps out and back (held in `pinned` → no arrival)
    // - tx 1 dropped (propagation churn: no arrival); tx 3 arrives; ours (tx 9) served before
    //   any listing
    validator.evict(two);
    polled(&balancer).await;
    validator.evict(one);
    validator.hear(&chain, two);
    validator.hear(&chain, three);
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
    let one = chain.submit(decoded(raw_transaction(1, 0).1));
    let two = chain.submit(decoded(raw_transaction(2, 0).1));
    let at = |height: u32| chain.at(h(height));
    let validator = Arc::new(chain.validator(at(10)));
    validator.hear(&chain, one);

    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"], None);
    let reader = view.subscriber();

    polled(&balancer).await;
    assert_eq!(reader.current().trusted_validators()[0].health, Health::Live, "listed its mempool");
    view.set_verified(Some(chain.verified(at(10))));

    validator.follow(&chain, at(11));
    validator.estimate(h(40));
    validator.listing(Err(GetMempoolListingError::Inactive));
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at(11))));
    let catching_up = reader.current();
    let held = (catching_up.best(), catching_up.held_by());
    assert_eq!(held, (Some(at(11)), TrustedValidatorSet::at([0])), "catching up: still the holder");
    let meta = &catching_up.trusted_validators()[0];
    let answered = (meta.health, meta.tip(), meta.stale_blocks(), meta.agreement);
    let expected = (Health::CatchingUp, Some(at(11)), Some(29), Agreement::Agreed);
    assert_eq!(answered, expected, "an answer, no failure");
    assert_eq!(balancer.members().borrow().rows[0].failures, 0);
    assert!(catching_up.mempool().is_none(), "no mempool served");
    assert_eq!(catching_up.sighting(&one), None, "its only listing retracted: dropped");

    validator.follow(&chain, at(12));
    validator.estimate(h(12));
    validator.listing(Ok(()));
    validator.evict(one);
    validator.hear(&chain, two);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at(12))));
    let caught_up = reader.current();
    assert_eq!(caught_up.trusted_validators()[0].health, Health::Live);
    assert_eq!(
        caught_up.mempool().expect("held").entries().map(|e| e.txid).collect::<Vec<_>>(),
        [two],
    );
    cancel.cancel();
}

/// - Tip = the verified best block, never a validator's claim
/// - Holders = every validator whose chain holds it (one = enough)
/// - Third claiming a far higher tip: moves nothing, drops out of the holders
/// - b, c unreachable at first (not yet read), each joining as it answers
#[tokio::test(start_paused = true)]
async fn a_claimed_higher_tip_moves_nothing_and_holders_are_whoever_holds_the_verified_block() {
    let mut chain = MockChain::regtest();
    let at_90 = chain.mine_empty(90);
    let agreed = chain.mine_empty(10);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(agreed))).collect();
    let tx7 = chain.submit(decoded(raw_transaction(7, 0).1));
    for validator in &validators {
        validator.metadata(Some(vec![outbound("seed-a:8233")]), None);
    }
    validators[0].hear(&chain, tx7);
    validators[1].reachable(&Port::ALL, false);
    validators[2].reachable(&Port::ALL, false);

    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let reader = view.subscriber();
    view.set_verified(Some(chain.verified(agreed)));

    // one validator holding the verified block = a tip (one admission proves validity)
    polled(&balancer).await;
    let first = reader.current();
    assert_eq!((first.best(), first.held_by()), (Some(agreed), TrustedValidatorSet::at([0])));
    let sighting = first.sighting(&tx7).expect("validator a reported it");
    let trusted = (sighting.trusted().count(), first.mempool_readers().count());
    assert_eq!(trusted, (1, 1), "b, c not yet read");
    assert!(sighting.timeline().all_trusted.is_some(), "every reader so far lists it");
    let servable: Vec<_> = first.arrivals(None).iter().map(|entry| entry.txid).collect();
    assert_eq!(servable, [tx7], "servable once a holder lists it");

    validators[1].reachable(&Port::ALL, true);
    polled(&balancer).await;
    assert_eq!(reader.current().held_by(), TrustedValidatorSet::at([0, 1]), "a and b hold it");

    // c claims a higher tip (fork from 90, never verified): moves nothing, holds nothing
    let claimed = chain.branch(at_90).mine_empty(30).tip();
    validators[2].follow(&chain, claimed);
    validators[2].hear(&chain, tx7);
    validators[2].metadata(Some(vec![outbound("seed-z:8233")]), None);
    validators[2].reachable(&Port::ALL, true);
    polled(&balancer).await;
    let pinned = reader.current();
    let held = (pinned.best(), pinned.held_by());
    assert_eq!(held, (Some(agreed), TrustedValidatorSet::at([0, 1])), "never the claim");
    assert_eq!(
        pinned.trusted_validators()[2].agreement,
        Agreement::Diverged,
        "holds neither 100 nor 97"
    );
    let trusted = pinned.sighting(&tx7).map(|sighting| sighting.trusted().count());
    let trusted = (trusted, pinned.mempool_readers().count());
    assert_eq!(trusted, (Some(2), 3), "a and c list it, of three read");
    let peers: Vec<(&str, Vec<PeerInfo>)> = pinned
        .trusted_validators()
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

/// Three validators holding the verified tip, no peers
///
/// - expired = refused by the precheck: nothing checked, nothing pushed
/// - one holder's `checkadmission` decides: admissible = `Ok` at once, `ours` (servable before
///   any listing, unpriced), then pushed to a trusted entry; refused = that refusal, nothing pushed
#[tokio::test(start_paused = true)]
async fn a_checked_submission_answers_at_once_on_the_check() {
    let mut chain = MockChain::regtest();
    let agreed = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(agreed))).collect();

    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let reader = view.subscriber();

    polled(&balancer).await;
    view.set_verified(Some(chain.verified(agreed)));
    assert!(!reader.current().held_by().is_empty(), "every one holds the verified tip");
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.calls().sends).collect() };
    let checks = || -> usize { validators.iter().map(|v| v.calls().checks).sum() };

    let (expired_txid, expired) = raw_transaction(1, 50);
    let refused = view.submit(expired).await;
    let Err(SubmitError::Rejected(SendRawTransactionError::Rejected { code, message })) = refused
    else {
        panic!("expiry 50 at tip 50 = invalid in block 51: {refused:?}");
    };
    assert!(code == -25 && message.contains("expiry height 50"), "{code}: {message}");
    assert_eq!((pushes(), checks()), (vec![0, 0, 0], 0), "precheck refuses before any ask");
    assert!(reader.current().sighting(&expired_txid).is_none());

    // a 5 s `sendrawtransaction` never delays the answer: it comes off the check
    for validator in &validators {
        validator.latency(&[Port::Send], Duration::from_secs(5));
    }
    let (txid, sent) = raw_transaction(9, 0);
    let asked = tokio::time::Instant::now();
    let accepted = view.submit(sent.clone()).await;
    assert_eq!(accepted.expect("one holder's check admits it"), txid);
    assert_eq!((asked.elapsed(), checks()), (Duration::ZERO, 1), "answered on the check");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(pushes().iter().sum::<usize>(), 1, "then relayed: one trusted entry (no peers)");
    for validator in &validators {
        validator.latency(&[Port::Send], Duration::ZERO);
    }

    let pinned = reader.current();
    let sighting = pinned.sighting(&txid).expect("the submission recorded it");
    assert!(sighting.ours());
    let trusted = (sighting.trusted().count(), pinned.mempool_readers().count());
    assert_eq!(trusted, (0, 3), "not polled since");
    assert_eq!(sighting.timeline().first_trusted, None);
    let servable = |view: &crate::ChainViewSnapshot| {
        view.mempool().expect("tip held").entries().find(|entry| entry.txid == txid)
    };
    let served = servable(&pinned).expect("`ours` is servable with zero sightings");
    let raw = Bytes::from(sent.clone());
    let unpriced = MempoolEntry { txid, raw, fee: None, projection: Projection::default() };
    assert_eq!(served, unpriced, "a wallet sees its own send before it propagates, unpriced");

    // a lists what it accepted (fee = what its bytes leave: 0): the first listing prices it
    polled(&balancer).await;
    let pinned = reader.current();
    let listed = servable(&pinned).expect("still servable");
    assert_eq!((listed.raw, listed.fee), (Bytes::from(sent), Some(zats(0))));
    let sighting = pinned.sighting(&txid).expect("held");
    assert_eq!((sighting.trusted().count(), pinned.mempool_readers().count()), (1, 3));
    let timeline = sighting.timeline();
    assert!(timeline.first_trusted.is_some_and(|at| at >= timeline.first_seen), "{timeline:?}");
    assert_eq!(timeline.all_trusted, None, "b, c have not listed it");

    // a refused check = the refusal at once: one check, nothing pushed
    for validator in &validators {
        let message = "too low fee".to_owned();
        validator.relay(Err(SendRawTransactionError::Rejected { code: -26, message }));
    }
    let too_low = SendRawTransactionError::Rejected { code: -26, message: "too low fee".into() };
    let before = (pushes(), checks());
    let rejected = view.submit(raw_transaction(8, 0).1).await;
    assert!(matches!(rejected, Err(SubmitError::Rejected(r)) if r == too_low), "the validator's");
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!((pushes(), checks()), (before.0, before.1 + 1), "never relayed");

    let garbage = view.submit(vec![9u8; 8]).await;
    assert!(matches!(garbage, Err(SubmitError::Rejected(SendRawTransactionError::Malformed(_)))));
    cancel.cancel();
}

/// Three validators holding the verified tip, none answering `checkadmission` (zcashd, stock
/// zebrad): each entry tried once; unanimous rejection = the rejection, none reachable = no answer
#[tokio::test(start_paused = true)]
async fn an_unchecked_submission_tries_every_entry_before_its_verdict() {
    let mut chain = MockChain::regtest();
    let agreed = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(agreed))).collect();
    let too_low = SendRawTransactionError::Rejected { code: -26, message: "too low fee".into() };
    for validator in &validators {
        validator.checks_admission(false);
        validator.relay(Err(too_low.clone()));
    }
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"], None);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(agreed)));
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.calls().sends).collect() };

    let rejected = view.submit(raw_transaction(7, 0).1).await;
    assert!(matches!(rejected, Err(SubmitError::Rejected(r)) if r == too_low), "the validator's");
    assert_eq!(pushes(), [1, 1, 1], "each entry tried once");

    // none reachable != a rejection (nothing learnt about the transaction)
    for validator in &validators {
        validator.reachable(&[Port::Send], false);
    }
    let sent = view.submit(raw_transaction(6, 0).1).await;
    assert!(matches!(sent, Err(SubmitError::Unreachable { attempted: 3, .. })), "{sent:?}");
    cancel.cancel();
}

/// Paused clock, every validator accepting, none gossiping on its own
///
/// - lone entry's listing ≠ spread → resubmitted to another after the threshold
/// - a never-entry validator lists it → job ends, nothing more pushed
#[tokio::test(start_paused = true)]
async fn an_unspread_submission_is_resubmitted_after_the_threshold_until_an_outsider_lists_it() {
    let mut chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(4).expect("nz"),
    };
    let view = view.with_submit_policy(policy);
    polled(&balancer).await;
    let pushes = || -> Vec<usize> { validators.iter().map(|v| v.calls().sends).collect() };
    let (txid, sent) = raw_transaction(5, 0);
    let trusted = || view.subscriber().current().sighting(&txid).expect("held").trusted().count();

    view.submit(sent.clone()).await.expect("the first entry accepts");
    let first = pushes();
    assert_eq!(first.iter().sum::<usize>(), 1);
    polled(&balancer).await;
    assert_eq!(trusted(), 1, "the entry lists it, of 3");

    // ≤ 1 s polled + 8 s: inside the threshold
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(pushes(), first, "inside the threshold: no resubmit");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second = pushes();
    assert_eq!(second.iter().sum::<usize>(), 2, "threshold passed unspread: a second entry");
    assert!(second.iter().all(|&n| n <= 1), "a different validator: {second:?}");

    // the one validator never pushed to hears it by gossip and lists it: spread
    let outsider = second.iter().position(|&n| n == 0).expect("one untried");
    chain.submit(decoded(sent));
    validators[outsider].hear(&chain, txid);
    polled(&balancer).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(pushes(), second, "spread past its entries: the job ended, nothing more pushed");
    assert_eq!(trusted(), 3);
    cancel.cancel();
}

/// Paused clock, two trusted validators, four live peers in four netgroups + one dead
///
/// - each peer's mempool polled: `peers: x/4` = its listers; four netgroups = no quorum, so a
///   peer listing alone holds nothing (a trusted listing does: `peers: 2/4, trusted: 1/2`)
/// - checked submission → `Ok` at once, then relayed through the peers, never pushed to a trusted
///   validator; unseen past the threshold = relayed again; a trusted listing = spread, job over
/// - unchecked + nobody relays: the budget spent on peer relays, then one trusted verdict
#[tokio::test(start_paused = true)]
async fn peers_relay_first_and_a_trusted_validator_gives_the_verdict_when_unchecked() {
    let mut chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..2).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232"], None);
    let peer = |b: u8| std::net::SocketAddr::from(([10, b, 0, 1], 8233));
    let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    let utxos = Arc::new(chain.ledger(chain.genesis()));
    let polling = view.peer_mempool(utxos).expect("peers configured");
    tokio::spawn(polling.run(cancel.clone()));
    let round = || tokio::time::sleep(PEER_MEMPOOL_POLL);
    let id = |txid: TxId| PeerTxId { txid, auth_digest: None };
    let validator_pushes = || validators.iter().map(|v| v.calls().sends).sum::<usize>();
    polled(&balancer).await;

    // two peers list it: no quorum, not held until a trusted validator lists it
    let gossiped = chain.submit(decoded(raw_transaction(1, 0).1));
    peers.list(peer(1), vec![id(gossiped)]);
    peers.list(peer(2), vec![id(gossiped)]);
    round().await;
    let pinned = view.subscriber().current();
    assert_eq!(pinned.sighting(&gossiped), None, "peer-listed only = not held");
    validators[0].hear(&chain, gossiped);
    polled(&balancer).await;
    let pinned = view.subscriber().current();
    let sighting = pinned.sighting(&gossiped).expect("held once listed");
    let listers = pinned.listings.listers(&gossiped).len();
    let trusted = (sighting.trusted().count(), pinned.mempool_readers().count());
    assert_eq!((listers, trusted), (2, (1, 2)));

    // checked by a holder: answered at once, then relayed through the peers only
    view.set_verified(Some(chain.verified(chain.genesis())));
    polled(&balancer).await;
    let (txid, sent) = raw_transaction(2, 0);
    let answered = view.submit(sent.clone()).await;
    assert_eq!(answered.expect("a holder's check admits it"), txid);
    tokio::task::yield_now().await;
    let checks = validators.iter().map(|v| v.calls().checks).sum::<usize>();
    assert_eq!((checks, peers.pushes(), validator_pushes()), (1, 1, 0), "one check, one relay");
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(peers.pushes(), 1, "inside the threshold: no relay again");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(peers.pushes(), 2, "threshold passed unseen: relayed again");
    peers.list(peer(3), vec![id(txid)]);
    chain.submit(decoded(sent));
    validators[1].hear(&chain, txid);
    round().await;
    polled(&balancer).await;
    let pinned = view.subscriber().current();
    let sighting = pinned.sighting(&txid).expect("held");
    assert!(sighting.ours());
    let listers = pinned.listings.listers(&txid).len();
    let trusted = (sighting.trusted().count(), pinned.mempool_readers().count());
    assert_eq!((listers, trusted), (1, (1, 2)));
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(peers.pushes(), 2, "a trusted validator never pushed to lists it: nothing more");
    assert_eq!(validator_pushes(), 0, "no trusted validator relayed it");

    // no `checkadmission` + nobody relays: the budget spent on peer relays, then one trusted verdict
    for validator in &validators {
        validator.checks_admission(false);
    }
    let (silent, sent) = raw_transaction(3, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent).await });
    tokio::time::sleep(Duration::from_secs(31)).await;
    let answered = within("the wallet answered", answer).await.expect("submission task");
    assert_eq!(answered.expect("the verdict accepts"), silent);
    assert_eq!(peers.pushes(), 2 + 3, "max_attempts relays through the peers first");
    assert_eq!(validator_pushes(), 1, "then exactly one trusted verdict");
    cancel.cancel();
}

/// Paused clock, one trusted validator, seven live peers in seven netgroups, coinbases at 1..=3
/// funding `a` (10,000), `bad` (5,000) and `parent` (10,000)
///
/// - `a` listed by the validator and every peer, then the validator down: still served, the same
///   sighting (handed over to the quorum, never dropped and re-added)
/// - every peer lists `a`, `parent`, `child` (spends `parent`'s output) and `bad` (pays 6,000 of
///   5,000): `parent` at its fee, `child` one round later (unknown prevout: retried, not refused),
///   `bad` never (a negative fee: refused once, never re-fetched while it stays listed)
/// - listed by 3 of 7 = kept (> 1/3), by 2 = gone from the view; the quorum still reads: an empty
///   mempool served
/// - a submission with no trusted validator: nothing to check with, one peer relay, answered once
///   6 of 7 list it; no longer listed = still served (ours) until the next block, then gone
#[tokio::test(start_paused = true)]
async fn with_every_trusted_validator_down_a_peer_quorum_keeps_the_mempool_and_takes_sends() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    for (n, zats) in [(1u8, 10_000), (2, 5_000), (3, 10_000)] {
        chain.mine(|b| b.coinbase(|c| c.txid([n; 32]).pay(&alice, zats)));
    }
    let tip = chain.mine_empty(2);
    let validator = Arc::new(chain.validator(tip));
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["a:8232"], None);
    let peer = |b: u8| std::net::SocketAddr::from(([10, b, 0, 1], 8233));
    let seven: Vec<std::net::SocketAddr> = (1..=7).map(peer).collect();
    let peers = Arc::new(MockPeers::new(seven.clone(), []));
    let policy = crate::SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    let funding = |n: u8| OutPoint::new([n; 32], 0);
    let polling = view.peer_mempool(Arc::new(chain.ledger(tip))).expect("peers configured");
    tokio::spawn(polling.run(cancel.clone()));
    let round = || tokio::time::sleep(PEER_MEMPOOL_POLL);
    let id = |txid: TxId| PeerTxId { txid, auth_digest: None };
    let reader = view.subscriber();
    let (a, a_raw) = raw_transparent(&[funding(1)], &[(&alice, 9_000)]);
    let (bad, bad_raw) = raw_transparent(&[funding(2)], &[(&alice, 6_000)]);
    let (parent, parent_raw) = raw_transparent(&[funding(3)], &[(&alice, 9_000)]);
    let (child, child_raw) =
        raw_transparent(&[OutPoint::new(parent.into(), 0)], &[(&alice, 8_500)]);
    let pending = [(a, &a_raw), (parent, &parent_raw), (child, &child_raw), (bad, &bad_raw)];
    for (txid, raw) in pending {
        peers.hold(id(txid), Bytes::from(raw.clone()));
    }
    let listing = |ids: &[TxId], on: &[std::net::SocketAddr]| {
        for peer in &seven {
            let listed =
                if on.contains(peer) { ids.iter().copied().map(id).collect() } else { vec![] };
            peers.list(*peer, listed);
        }
    };
    let served = || -> Vec<(TxId, Option<Zatoshis>)> {
        let view = reader.current();
        let mempool = view.mempool().expect("a quorum reads");
        mempool.entries().map(|entry| (entry.txid, entry.fee)).collect()
    };
    let sorted = |mut entries: Vec<(TxId, Option<Zatoshis>)>| {
        entries.sort();
        entries
    };
    let first_seen = |txid: &TxId| reader.current().sighting(txid).map(|s| s.timeline().first_seen);

    // listed by the validator and the quorum, then the validator gone: handed over, never dropped
    view.set_verified(Some(chain.verified(tip)));
    chain.submit(decoded(a_raw.clone()));
    validator.hear(&chain, a);
    listing(&[a], &seven);
    polled(&balancer).await;
    round().await;
    let listed = first_seen(&a).expect("listed by the validator");
    assert_eq!(served(), [(a, Some(zats(1_000)))]);
    validator.reachable(&Port::ALL, false);
    // `Down` (its listing retracted) after 10 failures on the 0.5 → 30 s ladder
    tokio::time::sleep(Duration::from_secs(300)).await;
    let down = reader.current().trusted_validators()[0].health;
    assert_eq!(down, Health::Down, "its listing retracted, not kept as `Degraded`");
    assert!(reader.current().held_by().is_empty(), "no trusted validator vouches");
    assert_eq!(served(), [(a, Some(zats(1_000)))], "the quorum keeps it");
    assert_eq!(first_seen(&a), Some(listed), "the same sighting throughout");

    listing(&[a, parent, child, bad], &seven);
    round().await;
    let parents = sorted(vec![(a, Some(zats(1_000))), (parent, Some(zats(1_000)))]);
    assert_eq!(sorted(served()), parents, "`child` waits on its parent; `bad` refused");
    round().await;
    let all =
        sorted(vec![(a, Some(zats(1_000))), (parent, Some(zats(1_000))), (child, Some(zats(500)))]);
    assert_eq!(sorted(served()), all, "`child` retried, admitted once its parent is held");
    round().await;
    round().await;
    let bad_fetches = peers.fetched().into_iter().filter(|fetched| fetched.txid == bad).count();
    assert_eq!(bad_fetches, 1, "refused once, never re-fetched while the quorum keeps listing it");
    let pinned = reader.current();
    let sighting = pinned.sighting(&a).expect("held");
    let listers = pinned.listings.listers(&a).len();
    let trusted = (sighting.trusted().count(), pinned.mempool_readers().count());
    assert_eq!((listers, trusted), (7, (0, 0)));

    listing(&[a, parent, child], &seven[..3]);
    round().await;
    assert_eq!(sorted(served()), all, "3 of 7 still list them: kept");
    listing(&[a, parent, child], &seven[..2]);
    round().await;
    assert_eq!(served(), [], "2 of 7: no longer kept; the quorum still reads, an empty mempool");
    let gone = [a, parent, child].map(|txid| first_seen(&txid));
    assert_eq!(gone, [None, None, None], "gone from the view");

    // a send with no trusted validator: a peer first; the quorum listing it = the answer
    let (sent, sent_raw) = raw_transparent(&[funding(1)], &[(&alice, 9_500)]);
    peers.hold(id(sent), Bytes::from(sent_raw.clone()));
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move { submitting.submit(sent_raw).await });
    tokio::task::yield_now().await;
    let asked = (peers.pushes(), validator.calls().sends, validator.calls().checks);
    assert_eq!(asked, (1, 0, 0), "no holder to check with: one peer relay");
    listing(&[sent], &seven[1..]);
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
        (0..3).map(|_| Arc::new(chain.validator(at_100))).collect();
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let reader = view.subscriber();
    let held_by = |positions: &[usize]| TrustedValidatorSet::at(positions.iter().copied());
    let agreements = || -> Vec<Agreement> {
        reader.current().trusted_validators().iter().map(|meta| meta.agreement).collect()
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
    // once, held by a alone (its poll's hold merged before its bytes are fetched)
    validators[0].follow(&chain, at_101);
    validators[0].latency(&[Port::MempoolBytes], Duration::from_secs(20));
    let unmined = chain.submit(decoded(raw_transaction(1, 0).1));
    validators[0].hear(&chain, unmined);
    polled(&balancer).await;
    view.set_verified(Some(chain.verified(at_101)));
    assert_eq!(tip(), Some((at_101, held_by(&[0]))), "one holder is enough, bytes or not");
    assert_eq!(agreements(), [Agreement::Agreed, Agreement::Behind, Agreement::Behind]);
    validators[0].latency(&[Port::MempoolBytes], Duration::ZERO);
    tokio::time::sleep(Duration::from_secs(21)).await;

    // a goes unreachable: no trusted validator holds 101 → no mempool (never a weaker answer)
    validators[0].reachable(&Port::ALL, false);
    polled(&balancer).await;
    let health = reader.current().trusted_validators()[0].health;
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
    let c = &pinned.trusted_validators()[2];
    assert_eq!((c.tip(), c.agreement), (Some(at_101), Agreement::Diverged));
    let links: usize = validators.iter().map(|v| v.calls().links).sum();
    assert_eq!(links, 0, "holding is asked by getblockhash in the poll: no header reads");
    cancel.cancel();
}

/// Peers + release ride the poll every 60 s; the push stream's state, every poll
///
/// - failed half → validator still live, last answer kept
/// - release halting within a week of the tip raises `ending`; an upgrade clears it
/// - stream up / down (the balancer's) shown from the next poll
#[tokio::test(start_paused = true)]
async fn metadata_rides_the_poll_and_a_failed_read_keeps_the_last_answer() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(10);
    let validator = Arc::new(chain.validator(tip));
    let release = |build: &str, halts: u32| NodeRelease {
        build: build.to_owned(),
        user_agent: format!("/Zebra:{}/", &build[1..]),
        end_of_service: EndOfService::At { height: h(halts) },
    };
    let inbound = PeerInfo { addr: "x:1".to_owned(), inbound: true };
    validator.metadata(
        Some(vec![outbound("seed-a:8233"), inbound]),
        Some(release("v6.4.2", 10 + 4_960)),
    );
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["one:8232"], None);
    let reader = view.subscriber();
    let peers = || -> Vec<PeerInfo> {
        reader.current().trusted_validators()[0].peers.iter().cloned().collect()
    };
    let build =
        || reader.current().trusted_validators()[0].release.as_ref().map(|r| r.build.clone());
    let streaming = || reader.current().trusted_validators()[0].streaming;
    let one = TrustedValidatorSet::at([0]);

    polled(&balancer).await;
    let first = peers();
    assert_eq!(first.len(), 2);
    assert_eq!(build().as_deref(), Some("v6.4.2"));
    let pinned = reader.current();
    assert_eq!(pinned.trusted_validators()[0].blocks_to_end_of_service(), Some(4_960));
    assert_eq!(pinned.alarms().ending(), one, "halts within a week of its tip");

    validator.metadata(None, None);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let health = (
        reader.current().trusted_validators()[0].health,
        balancer.members().borrow().rows[0].failures,
    );
    assert_eq!(health, (Health::Live, 0), "a metadata failure is not a poll failure");
    assert_eq!((peers(), build().as_deref()), (first, Some("v6.4.2")), "last answers kept");

    validator.metadata(Some(Vec::new()), Some(release("v6.5.0", 3_700_000)));
    polled(&balancer).await;
    assert_eq!(build().as_deref(), Some("v6.4.2"), "read once per refresh, not per poll");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(peers(), [], "a fresh answer replaces it (isolated = empty, not an error)");
    assert_eq!(build().as_deref(), Some("v6.5.0"));
    assert_eq!(reader.current().alarms().ending(), TrustedValidatorSet::default(), "upgraded");

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
    let mut chain = MockChain::regtest();
    let genesis = chain.genesis();
    let validator = Arc::new(chain.validator(genesis));
    let tx1 = chain.submit(decoded(raw_transaction(1, 0).1));
    validator.hear(&chain, tx1);
    let (view, _, cancel) = running(&[Arc::clone(&validator)], &["one:8232"], None);
    view.set_verified(Some(chain.verified(genesis)));
    let reader = view.subscriber();
    let state = || reader.current().trusted_validators()[0].health;
    let serves_tx1 = || {
        let pinned = reader.current();
        pinned.mempool().is_some_and(|mempool| mempool.entries().any(|entry| entry.txid == tx1))
    };

    until("live", serves_tx1).await;

    validator.reachable(&Port::ALL, false);
    until("down", || state() == Health::Down).await;
    let pinned = reader.current();
    assert_eq!(pinned.held_by(), TrustedValidatorSet::default(), "no holder left");
    assert!(pinned.mempool().is_none(), "fail closed");
    let listing = pinned.sighting(&tx1).map(|sighting| sighting.trusted()).unwrap_or_default();
    let trusted = (listing.count(), pinned.mempool_readers().count());
    assert_eq!(trusted, (0, 0), "retracted, and none left reading");

    validator.reachable(&Port::ALL, true);
    until("back", serves_tx1).await;
    assert_eq!(state(), Health::Live);
    cancel.cancel();
}

/// `done` checked every 20 ms (virtual) for 10 min, at idle points only (paused clock: every task
/// runs before a timer fires, blocking work holds the clock)
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..30_000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never {what}");
}

/// Finality stall alarm (telemetry, depth 10): finalized = best − depth never raises it; a block
/// `depth` deep owed finality raises it only past 60 s; the finalized tip moving clears it
#[tokio::test(start_paused = true)]
async fn finality_stalled_rises_after_a_minute_unmoved_and_clears_when_finalized_moves() {
    let mut chain = MockChain::regtest();
    let tip_30 = chain.mine_empty(30);
    let tip_45 = chain.mine_empty(15);
    let validator = Arc::new(chain.validator(tip_45));
    let (view, _balancer, cancel) = running(&[validator], &["a:8232"], None);
    let reader = view.subscriber();
    let stalled = || reader.current().alarms().finality_paused();

    view.set_verified(Some(chain.verified_finalized(tip_30, h(20))));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(!stalled(), "finalized = best − depth: nothing owed");
    view.set_verified(Some(chain.verified_finalized(tip_45, h(20))));
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert!(!stalled(), "unmoved 59 s: not yet");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(stalled(), "unmoved past 60 s, 25 non-finalized");
    view.set_verified(Some(chain.verified_finalized(tip_45, h(35))));
    assert!(!stalled(), "finalized moved: cleared at once");
    cancel.cancel();
}

/// Real regtest header bytes through header sync, two trusted validators, depth 10
///
/// - a at 5,000: the chain anchors at its claim − max(depth, CONTEXT) (4,887), never reading
///   history below, then follows a's 113 headers; finalized = 4,990, held by a (b's claim 3,000 =
///   below finalized, held by definition, nothing fetched)
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

    let validators = [a, at_3_000].map(|tip| Arc::new(chain.validator(tip)));
    let (view, balancer, cancel) = running(&validators, &["a:8232", "b:8232"], None);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let verified = sync.subscribe();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));

    until("anchored below a's claim, a's tip followed", || published() == Some((a, at_4_990)))
        .await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(a), TrustedValidatorSet::at([0])), "a holds the tip, b behind it");
    let first = verified.borrow().clone().expect("a VerifiedChain");
    let anchored = (first.hash_at(at_4_887.height), first.hash_at(at_4_886.height));
    assert_eq!(anchored, (Some(at_4_887.hash), None), "no history below the anchor");
    assert!(first.on_best(at_3_000), "b's claim: below finalized, finalized by definition");
    let fetched: Vec<usize> = validators.iter().map(|validator| validator.calls().links).collect();
    assert_eq!(fetched, [114, 0], "the anchor + 113 headers from a, nothing from b");
    assert!(!reader.current().alarms().finality_paused());

    validators[1].follow(&chain, b);
    polled(&balancer).await;
    until("b's heavier fork followed", || reader.current().best() == Some(b)).await;
    let held = (reader.current().best(), reader.current().held_by());
    assert_eq!(held, (Some(b), TrustedValidatorSet::at([1])), "the work, not the first");
    until("b's chain published", || published().map(|(best, _)| best) == Some(b)).await;
    assert_eq!(first.best(), a, "a published chain never changes (H5)");

    cancel.cancel();
    syncing.await.expect("header sync never panics");
}

/// A validator far ahead, a poll forced after every publish: the chain re-anchors at its claim
/// − depth instead of syncing headers toward it
///
/// - a at 20: anchored at genesis (20 − max(depth, CONTEXT), saturated), followed to 20,
///   finalized 10
/// - a jumps to 20,500 (past the ceiling: depth + `HEADER_BATCH` above finalized) → re-anchored at
///   20,387, followed to 20,500, finalized 20,490: 135 headers fetched in all, none of the 20,366
///   between
/// - every published chain: best − finalized ≤ depth + `HEADER_BATCH` (H9), finality alarm
///   down; the view's tip held by a
#[tokio::test(start_paused = true)]
async fn a_validator_far_ahead_re_anchors_the_chain_instead_of_syncing_toward_it() {
    use zaino_header_chain::HeaderChain;

    let mut chain = MockChain::regtest();
    let at_20 = chain.mine_empty(20);
    let tip = chain.mine_empty(20_480);
    let (genesis, at_10) = (chain.genesis(), chain.at(h(10)));
    let (at_20_387, at_20_490) = (chain.at(h(20_387)), chain.at(h(20_490)));
    let validator = Arc::new(chain.validator(at_20));
    let (view, balancer, cancel) = running(&[Arc::clone(&validator)], &["a:8232"], None);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let mut verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));

    let mut followed = Vec::new();
    loop {
        let changed = tokio::time::timeout(Duration::from_secs(60), verified.changed()).await;
        changed.expect("header sync progresses").expect("header sync running");
        let (best, finalized_tip) = {
            let published = verified.borrow_and_update();
            let published = published.as_ref().expect("a chain once published");
            (published.best(), published.finalized_tip())
        };
        let non_finalized = u32::from(best.height) - u32::from(finalized_tip.height);
        let bound = DEPTH + crate::headers::HEADER_BATCH;
        assert!(non_finalized <= bound, "{best:?}: H9, {non_finalized} non-finalized > {bound}");
        assert!(!reader.current().alarms().finality_paused(), "{best:?}: alarm");
        followed.push((best, finalized_tip));
        balancer.pushed(v(0), Push::Changed);
        if (best, finalized_tip) == (at_20, at_10) {
            validator.follow(&chain, tip);
        }
        if (best, finalized_tip) == (tip, at_20_490) {
            break;
        }
    }
    let finalized_tips: Vec<BlockRef> =
        followed.iter().map(|(_, finalized_tip)| *finalized_tip).collect();
    let anchors_and_finalized = [genesis, at_10, at_20_387, at_20_490];
    let jumped =
        finalized_tips.iter().all(|finalized_tip| anchors_and_finalized.contains(finalized_tip));
    assert!(jumped, "finalized tip = an anchor or its run's end only: {finalized_tips:?}");
    let links = validator.calls().links;
    assert_eq!(links, 1 + 20 + 1 + 113, "two anchors + their runs, nothing between");
    until("the view's tip, held", || reader.current().held_by() == TrustedValidatorSet::at([0]))
        .await;
    cancel.cancel();
}

/// Two trusted validators at 30, depth 10; a then serves a longer chain forked at 5, below our
/// finalized tip
///
/// - a's forked claim: given up on (never fetched again while it stands), `Diverged`, never the
///   tip (no re-anchor onto a chain leaving ours below finalized)
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
    let validators = [at_30, at_30].map(|tip| Arc::new(chain.validator(tip)));
    let (view, _balancer, cancel) = running(&validators, &["a:8232", "b:8232"], None);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));
    let followed = async |tip: BlockRef, finalized_tip: BlockRef| {
        let asked = tokio::time::Instant::now();
        until("followed", || published() == Some((tip, finalized_tip))).await;
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
    let standing = (pinned.best(), pinned.trusted_validators()[0].agreement);
    assert_eq!(standing, (Some(at_32), Agreement::Diverged), "below finalized: never followed");
    cancel.cancel();
}

/// One trusted validator + the peer set, both at 40, depth 10, real regtest headers:
/// - a anchors and leads to 40, finalized 30
/// - a down, the network moves on to 60: the peer set's 41..=60 each pass every rule; best = 60,
///   never vouched, so finalized stops at 40 = a's last vouch (best − depth would be 50), best held
///   by no one
/// - a back at 60: it vouches the peers' chain, finalized = 50, held by a
#[tokio::test(start_paused = true)]
async fn peers_extend_the_tip_past_a_down_validator_and_finality_waits_for_its_return() {
    use zaino_header_chain::{HeaderChain, Params};

    let mut chain = MockChain::regtest();
    let at_40 = chain.mine_empty(40);
    let at_60 = chain.mine_empty(20);
    let (at_30, at_50) = (chain.at(h(30)), chain.at(h(50)));
    let peer_set = Arc::new(MockPeerSet::new(&chain, at_60));
    peer_set.hold(h(40));
    let validator = Arc::new(chain.validator(at_40));
    let transport = Arc::clone(&peer_set) as _;
    let (view, _balancer, cancel) =
        running(&[Arc::clone(&validator)], &["a:8232"], Some(transport));
    let reader = view.subscriber();
    let rules = Params::regtest(regtest());
    let sync = view.header_sync(HeaderChain::new(depth())).with_peers(rules);
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));

    until("anchored, a followed", || published() == Some((at_40, at_30))).await;

    validator.reachable(&Port::ALL, false);
    peer_set.hold(h(60));
    until("the peers' tip, unvouched", || published() == Some((at_60, at_40))).await;
    until("best held by no one", || reader.current().held_by() == TrustedValidatorSet::default())
        .await;
    assert_eq!(reader.current().best(), Some(at_60));

    validator.follow(&chain, at_60);
    validator.reachable(&Port::ALL, true);
    until("a vouches the peers' chain", || published() == Some((at_60, at_50))).await;
    until("held by a", || reader.current().held_by() == TrustedValidatorSet::at([0])).await;
    cancel.cancel();
}

/// a at 40 (finalized 40 once best ≥ 50), the peer set's best path above it; each row = what the
/// peers' path ends in, where the tip stops, whether the peers were blamed:
/// - 2,100 good headers: the ceiling (finalized + depth + `HEADER_BATCH` = 2,050) exactly
/// - block 45 from the future: 44, no blame (only the clock is behind)
/// - block 45 with nBits the rules refuse: 44, each refusal a misanswer on the peer set
#[tokio::test(start_paused = true)]
async fn peer_headers_stop_at_the_ceiling_wait_out_the_clock_and_a_broken_rule_is_a_misanswer() {
    use metrics_util::debugging::DebuggingRecorder;
    use zaino_header_chain::{HeaderChain, Params};

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
    for (row, path_end, (tip, finalized_tip), blamed) in rows {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recording = metrics::set_default_local_recorder(&recorder);
        let mut chain = MockChain::regtest().varied_work();
        let at_40 = chain.mine_empty(40);
        let validator = Arc::new(chain.validator(at_40));
        chain.mine_empty(4);
        path_end(&mut chain);
        let peers_tip = chain.mine_empty(1);
        let best: Vec<BlockRef> =
            chain.blocks(peers_tip).iter().map(|block| block.block_ref()).collect();
        let peer_set = Arc::new(MockPeerSet::new(&chain, peers_tip));
        let (view, _balancer, cancel) = running(&[validator], &["a:8232"], Some(peer_set));
        let sync =
            view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(regtest()));
        let verified = sync.subscribe();
        tokio::spawn(sync.run(cancel.clone()));
        let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));

        let expected = Some((best[tip as usize], best[finalized_tip as usize]));
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
/// `inv`s: finalized reaches best − depth = 50 on the peers' word once more than 5 netgroups
/// carry it
/// - 10.1–10.5 (two in 10.5) announce 60: five netgroups, finalized stays 40
/// - 10.7 announces a block off our chain; 10.8 (not live: inbound) announces 60; 10.6 announces
///   45 (ours, but below 50: its chain may leave ours after 45): none counts
/// - 10.6 then announces 55 (ours, above 50): six netgroups → finalized 50, a still down
#[tokio::test(start_paused = true)]
async fn six_netgroups_carrying_the_chain_finalize_it_while_no_trusted_validator_vouches() {
    use zaino_header_chain::{HeaderChain, Params};

    let mut chain = MockChain::regtest();
    let at_40 = chain.mine_empty(40);
    let at_60 = chain.mine_empty(20);
    let (at_30, at_45) = (chain.at(h(30)), chain.at(h(45)));
    let (at_50, at_55) = (chain.at(h(50)), chain.at(h(55)));
    let elsewhere = chain.fork(h(45)).mine_empty(3).tip();
    let peer_set = Arc::new(MockPeerSet::new(&chain, at_60));
    peer_set.hold(h(40));
    let validator = Arc::new(chain.validator(at_40));
    let transport = Arc::clone(&peer_set) as _;
    let (view, _balancer, cancel) =
        running(&[Arc::clone(&validator)], &["a:8232"], Some(transport));
    let peer = |b: u8, host: u8| std::net::SocketAddr::from(([10, b, 0, host], 8233));
    let live = [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2), (6, 1), (7, 1)];
    let announcing = Arc::new(MockPeers::new(live.map(|(b, host)| peer(b, host)), []));
    let view = view.with_peers(announcing.clone());
    tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
    let sync = view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(regtest()));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));

    until("anchored, a followed", || published() == Some((at_40, at_30))).await;
    validator.reachable(&Port::ALL, false);
    peer_set.hold(h(60));
    until("the peers' tip, unvouched", || published() == Some((at_60, at_40))).await;

    for (b, host) in [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2)] {
        announcing.announce_block(peer(b, host), at_60.hash);
    }
    announcing.announce_block(peer(7, 1), elsewhere.hash);
    announcing.announce_block(peer(8, 1), at_60.hash);
    announcing.announce_block(peer(6, 1), at_45.hash);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(published(), Some((at_60, at_40)), "five netgroups: finalized unmoved");

    announcing.announce_block(peer(6, 1), at_55.hash);
    until("finalized on six netgroups' word", || published() == Some((at_60, at_50))).await;
    let held_by = view.subscriber().current().held_by();
    assert_eq!(held_by, TrustedValidatorSet::default(), "a still down: vouched nothing");
    cancel.cancel();
}
