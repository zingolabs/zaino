//! Scenarios against `MockValidator`s (+ `MockPeers`), through the real balancer, on a paused clock
//!
//! - Each poll = the balancer's (every second, or woken)
//! - [`next_poll_merged`] waits out one per member

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::{HeaderChain, HeaderChainSnapshot, Params, CONTEXT};
use zaino_primitives::types::{BlockRef, FinalityDepth, PeerTxId};
use zaino_source::testing::{
    h, p2pkh, raw_transaction, raw_transparent, within, MockChain, MockValidator,
};
use zaino_source::{
    EndOfService, GetMempoolListingError, NodeRelease, PeerInfo, SendRawTransactionError,
    Telemetry, ZcashLegacyRpcClient,
};
use zaino_traffic::testing::MockPeerSet;
use zaino_traffic::{
    Health, Limits, PeerTransport, Peers, Push, TrafficBalancer, TrustedValidator,
    TrustedValidatorId,
};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{BlockHash, TxId};
use zcash_transparent::bundle::OutPoint;

use crate::alarm_thresholds::FINALITY_STALL;
use crate::header_sync::{HEADER_BATCH, PEER_POLL};
use crate::peer_mempool::PEER_MEMPOOL_POLL;
use crate::testing::MockPeers;
use crate::{
    Agreement, Alarm, ChainView, MempoolEntry, Projection, SubmitError, SubmitPolicy,
    TrustedValidatorSet,
};

const DEPTH: u32 = 10;
/// zaino-traffic's poll cadence (live, not streaming)
const POLL: Duration = Duration::from_secs(1);
/// zaino-traffic's metadata refresh: peers + release ride one poll a minute
const METADATA_EVERY: Duration = Duration::from_secs(60);

/// View over `validators` (`addresses` in order, priority 0, 8 connections each) + `peer_set`
/// (4 in flight), depth [`DEPTH`], regtest's schedule; balancer driver + poll merge spawned
fn view_merging_polls(
    validators: &[Arc<MockValidator>],
    addresses: &[&str],
    peer_set: Option<Arc<dyn PeerTransport>>,
) -> (ChainView<ZcashLegacyRpcClient>, TrafficBalancer<ZcashLegacyRpcClient>, CancellationToken) {
    assert_eq!(validators.len(), addresses.len(), "one address per validator");
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = validators.iter().zip(addresses).map(|(validator, address)| TrustedValidator {
        address: (*address).to_owned(),
        source: Arc::new(validator.client()),
        priority: 0,
        limits,
    });
    let peers = peer_set.map(|transport| Peers { transport, in_flight: 4 });
    let (balancer, driver) = TrafficBalancer::new(trusted.collect(), peers);
    let shutdown = CancellationToken::new();
    let view = ChainView::new(balancer.clone(), depth(), regtest(), shutdown);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_merge().run(cancel.clone()));
    (view, balancer, cancel)
}

/// Every member polled again (cadence, ladder or a wake) and that poll merged (paused clock: the
/// 1 ms timer fires once every task idles)
async fn next_poll_merged(balancer: &TrafficBalancer<ZcashLegacyRpcClient>) {
    let members = balancer.addresses().len();
    let mut watches: Vec<_> = (0..members).map(|at| balancer.observe(v(at))).collect();
    for watch in &mut watches {
        watch.borrow_and_update();
    }
    for watch in &mut watches {
        within("a member polled", watch.changed()).await.expect("driver running");
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
}

/// Member `at` failed down the ladder to `Down` (each failed poll ≤ 30 s after the last), merged
async fn down_and_merged(balancer: &TrafficBalancer<ZcashLegacyRpcClient>, at: usize) {
    let mut polls = balancer.observe(v(at));
    while polls.borrow_and_update().as_ref().map(|polled| polled.health) != Some(Health::Down) {
        within("the next failed poll", polls.changed()).await.expect("driver running");
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
}

/// `done` within `within`'s 60 s, checked every 20 ms (paused clock: at idle points only)
async fn eventually(what: &str, done: impl Fn() -> bool) {
    within(what, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
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

fn depth() -> FinalityDepth {
    FinalityDepth::new(std::num::NonZeroU32::new(DEPTH).expect("nz"))
}

/// `MockChain::regtest()`'s: every upgrade through NU6.3 at 1
fn regtest() -> zcash_protocol::local_consensus::LocalNetwork {
    MockChain::regtest().schedule().upgrades
}

fn peer_tx_id(txid: TxId) -> PeerTxId {
    PeerTxId { txid, auth_digest: None }
}

/// Peer `b` in its own netgroup 10.b
fn peer(b: u8) -> SocketAddr {
    SocketAddr::from(([10, b, 0, 1], 8233))
}

/// Coinbases at 1..=2 fund txs 1, 2 (fee n · 1 000), both listed
#[tokio::test(start_paused = true)]
async fn the_mempool_opens_once_a_polled_validator_holds_the_verified_best_each_entry_at_its_fee() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = [1, 2].map(|_| {
        let block = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 10_000)));
        chain.output(block, 0, 0)
    });
    let tip = chain.mine_empty(8);
    let [(one, one_raw), (two, two_raw)] = [1u64, 2].map(|n| {
        raw_transparent(&[funded[n as usize - 1].clone()], &[(&alice, 10_000 - n * 1_000)])
    });
    chain.submit_raw(one_raw.clone());
    chain.submit_raw(two_raw.clone());
    let validator = Arc::new(chain.validator(tip));
    validator.hear(&chain, one);
    validator.hear(&chain, two);
    let (view, balancer, cancel) = view_merging_polls(&[validator], &["one:8232"], None);
    let reader = view.subscriber();

    let unverified = reader.current();
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
    let unpolled = reader.current();
    next_poll_merged(&balancer).await;
    let polled = reader.current();

    assert!(unverified.mempool().is_none(), "no verified best: no mempool");
    assert_eq!(unpolled.best(), Some(tip), "the header chain's best, before any poll");
    assert!(unpolled.mempool().is_none(), "verified, no holder polled: no mempool");
    assert_eq!(polled.held_by(), TrustedValidatorSet::at([0]), "polled at the best: its holder");
    let entry = |txid, raw: &[u8], fee| MempoolEntry {
        txid,
        raw: Bytes::from(raw.to_vec()),
        fee: Some(zats(fee)),
        projection: Projection::default(),
    };
    let mut listed = vec![entry(one, &one_raw, 1_000), entry(two, &two_raw, 2_000)];
    listed.sort_by_key(|entry| entry.txid);
    let entries: Vec<_> = polled.mempool().expect("held").entries().collect();
    assert_eq!(entries, listed, "each entry at its validator's fee");
    cancel.cancel();
}

/// Listing 1 and 2 when pinned, then 2 out and back, 1 dropped, 3 heard
#[tokio::test(start_paused = true)]
async fn arrivals_since_a_pinned_view_are_each_newly_held_transaction_once() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let [one, two, three] = [1, 2, 3].map(|n| chain.submit_raw(raw_transaction(n, 0).1));
    let validator = Arc::new(chain.validator(tip));
    validator.hear(&chain, one);
    validator.hear(&chain, two);
    let (view, balancer, cancel) =
        view_merging_polls(&[Arc::clone(&validator)], &["one:8232"], None);
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
    next_poll_merged(&balancer).await;
    let pinned = view.subscriber().current();

    validator.evict(two);
    next_poll_merged(&balancer).await;
    validator.evict(one);
    validator.hear(&chain, two);
    validator.hear(&chain, three);
    next_poll_merged(&balancer).await;

    let txids = |entries: Vec<MempoolEntry>| -> Vec<TxId> {
        let mut txids: Vec<TxId> = entries.iter().map(|entry| entry.txid).collect();
        txids.sort();
        txids
    };
    let mut held = vec![one, two];
    held.sort();
    assert_eq!(txids(pinned.arrivals(None)), held, "pinned: 1 and 2 held");
    let now = view.subscriber().current();
    assert_eq!(txids(now.arrivals(Some(&pinned))), [three], "since the pin: 3 alone");
    assert_eq!(now.arrivals(Some(&now)), [], "nothing since itself");
    cancel.cancel();
}

/// Mempool off below the network tip → `CatchingUp`; its clock estimate 40 against tip 11
#[tokio::test(start_paused = true)]
async fn a_catching_up_validator_holds_its_tip_serves_no_mempool_and_lists_again_once_active() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(12);
    let one = chain.submit_raw(raw_transaction(1, 0).1);
    let two = chain.submit_raw(raw_transaction(2, 0).1);
    let at = |height: u32| chain.at(h(height));
    let validator = Arc::new(chain.validator(at(10)));
    validator.hear(&chain, one);
    let (view, balancer, cancel) =
        view_merging_polls(&[Arc::clone(&validator)], &["one:8232"], None);
    let reader = view.subscriber();
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at(10))));
    let live = reader.current().trusted_validators()[0].health;

    validator.follow(&chain, at(11));
    validator.estimate(h(40));
    validator.listing(Err(GetMempoolListingError::Inactive));
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at(11))));
    let catching_up = reader.current();

    validator.follow(&chain, at(12));
    validator.estimate(h(12));
    validator.listing(Ok(()));
    validator.evict(one);
    validator.hear(&chain, two);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at(12))));
    let caught_up = reader.current();

    assert_eq!(live, Health::Live, "listed its mempool: live");
    let meta = &catching_up.trusted_validators()[0];
    assert_eq!(meta.health, Health::CatchingUp, "an answer, no failure (a failure = Degraded)");
    assert_eq!(catching_up.held_by(), TrustedValidatorSet::at([0]), "catching up: holds 11");
    assert_eq!(meta.tip(), Some(at(11)), "its tip");
    assert_eq!(meta.stale_blocks(), Some(29), "its estimate 40 − its tip 11");
    assert_eq!(catching_up.agreements().next(), Some(Agreement::Agreed), "claim = best");
    assert!(catching_up.mempool().is_none(), "catching up: no mempool served");
    assert_eq!(catching_up.sighting(&one), None, "its only listing of 1 retracted");
    assert_eq!(caught_up.trusted_validators()[0].health, Health::Live, "active again");
    let listed: Vec<TxId> = caught_up.mempool().expect("held").entries().map(|e| e.txid).collect();
    assert_eq!(listed, [two], "active again: its listing served");
    cancel.cancel();
}

/// a answers, b and c unreachable at first; a lists tx 7
#[tokio::test(start_paused = true)]
async fn holders_are_whoever_holds_the_verified_best_each_joining_as_it_answers() {
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(100);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    let tx7 = chain.submit_raw(raw_transaction(7, 0).1);
    validators[0].hear(&chain, tx7);
    validators[1].down(true);
    validators[2].down(true);
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let reader = view.subscriber();
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));

    next_poll_merged(&balancer).await;
    let first = reader.current();
    validators[1].down(false);
    next_poll_merged(&balancer).await;
    let joined = reader.current();

    assert_eq!(first.best(), Some(best), "the verified best");
    assert_eq!(first.held_by(), TrustedValidatorSet::at([0]), "a alone answers: one holder");
    let listing = first.sighting(&tx7).map(|sighting| sighting.trusted());
    assert_eq!(listing, Some(TrustedValidatorSet::at([0])), "tx 7's trusted listers");
    assert_eq!(first.mempool_readers(), TrustedValidatorSet::at([0]), "mempools read: a's");
    let servable: Vec<TxId> = first.arrivals(None).iter().map(|entry| entry.txid).collect();
    assert_eq!(servable, [tx7], "servable once a holder lists it");
    assert_eq!(joined.held_by(), TrustedValidatorSet::at([0, 1]), "b answers: joins");
    cancel.cancel();
}

/// a, b at 100; c claims 120 on a fork from 90 (never verified); a and c list tx 7
#[tokio::test(start_paused = true)]
async fn a_claimed_higher_tip_off_our_chain_moves_nothing_and_holds_nothing() {
    let mut chain = MockChain::regtest();
    let at_90 = chain.mine_empty(90);
    let best = chain.mine_empty(10);
    let claimed = chain.branch(at_90).mine_empty(30).tip();
    let validators: Vec<Arc<MockValidator>> =
        [best, best, claimed].map(|tip| Arc::new(chain.validator(tip))).to_vec();
    let tx7 = chain.submit_raw(raw_transaction(7, 0).1);
    validators[0].hear(&chain, tx7);
    validators[2].hear(&chain, tx7);
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));

    next_poll_merged(&balancer).await;

    let pinned = view.subscriber().current();
    assert_eq!(pinned.best(), Some(best), "the verified 100, never c's claim at 120");
    assert_eq!(pinned.held_by(), TrustedValidatorSet::at([0, 1]), "c's 100 is off our chain");
    assert_eq!(
        pinned.agreements().nth(2),
        Some(Agreement::Diverged),
        "c: neither 100 nor above it"
    );
    let listing = pinned.sighting(&tx7).map(|sighting| sighting.trusted());
    assert_eq!(listing, Some(TrustedValidatorSet::at([0, 2])), "c's listing still counts");
    assert_eq!(pinned.mempool_readers(), TrustedValidatorSet::at([0, 1, 2]), "all three read");
    cancel.cancel();
}

/// Tip 50: expiry 50 = invalid in block 51
#[tokio::test(start_paused = true)]
async fn the_precheck_refuses_an_expired_or_undecodable_send_before_any_ask() {
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));
    let (expired_txid, expired) = raw_transaction(1, 50);

    let expired = view.validate_transaction(expired).await;
    let garbage = view.validate_transaction(vec![9u8; 8]).await;

    let Err(SubmitError::Rejected(SendRawTransactionError::Rejected { code, message })) = &expired
    else {
        panic!("expiry 50 at tip 50: a −25 rejection, got {expired:?}");
    };
    assert_eq!(*code, -25, "expired: {message}");
    assert!(message.contains("expiry height 50"), "names the expiry: {message}");
    let malformed =
        matches!(garbage, Err(SubmitError::Rejected(SendRawTransactionError::Malformed(_))));
    assert!(malformed, "8 garbage bytes: {garbage:?}");
    let asked: usize =
        validators.iter().map(|v| v.calls("checkadmission") + v.calls("sendrawtransaction")).sum();
    assert_eq!(asked, 0, "nothing checked or pushed");
    assert!(view.subscriber().current().sighting(&expired_txid).is_none(), "never held");
    cancel.cancel();
}

/// Three holders, every `sendrawtransaction` `SLOW_RELAY` slow, no peers
#[tokio::test(start_paused = true)]
async fn a_checked_send_answers_at_once_is_served_unpriced_and_is_relayed_to_one_trusted_entry() {
    const SLOW_RELAY: Duration = Duration::from_secs(5);
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));
    for validator in &validators {
        validator.latency(&["sendrawtransaction"], SLOW_RELAY);
    }
    let (txid, sent) = raw_transaction(9, 0);
    let asked = Instant::now();

    let validated = view.validate_transaction(sent.clone()).await.expect("one holder admits it");
    let answered = view.broadcast_transaction(validated).await;

    let waited = asked.elapsed();
    let served =
        view.subscriber().current().mempool().expect("held").entries().find(|e| e.txid == txid);
    tokio::time::sleep(SLOW_RELAY + Duration::from_secs(1)).await;
    let calls = |method| validators.iter().map(|v| v.calls(method)).sum::<usize>();
    assert_eq!(answered.expect("answered on the check"), txid, "the txid of the bytes");
    assert_eq!(waited, Duration::ZERO, "answered on the check, not after the {SLOW_RELAY:?} relay");
    assert_eq!(calls("checkadmission"), 1, "one holder checked it");
    let unpriced =
        MempoolEntry { txid, raw: Bytes::from(sent), fee: None, projection: Projection::default() };
    assert_eq!(served, Some(unpriced), "ours, served before any listing, unpriced");
    assert_eq!(calls("sendrawtransaction"), 1, "relayed: one trusted entry (no peers)");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_refused_check_is_the_answer_at_once_and_nothing_is_relayed() {
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    let too_low = SendRawTransactionError::Rejected { code: -26, message: "too low fee".into() };
    for validator in &validators {
        validator.relay(Err(too_low.clone()));
    }
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));

    let rejected = view.validate_transaction(raw_transaction(8, 0).1).await;

    tokio::time::sleep(Duration::from_millis(1)).await;
    let refusal = matches!(&rejected, Err(SubmitError::Rejected(said)) if *said == too_low);
    assert!(refusal, "the holder's refusal: {rejected:?}");
    let calls = |method| validators.iter().map(|v| v.calls(method)).sum::<usize>();
    assert_eq!(calls("checkadmission"), 1, "one check");
    assert_eq!(calls("sendrawtransaction"), 0, "never relayed");
    cancel.cancel();
}

/// No `checkadmission` anywhere (zcashd, stock zebrad), every entry rejecting
#[tokio::test(start_paused = true)]
async fn an_unchecked_send_every_entry_rejects_is_that_rejection_after_one_push_each() {
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    let too_low = SendRawTransactionError::Rejected { code: -26, message: "too low fee".into() };
    for validator in &validators {
        validator.lacks(&["checkadmission"]);
        validator.relay(Err(too_low.clone()));
    }
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));

    let validated = view.validate_transaction(raw_transaction(7, 0).1).await.expect("none judges");
    let rejected = view.broadcast_transaction(validated).await;

    let refusal = matches!(&rejected, Err(SubmitError::Rejected(said)) if *said == too_low);
    assert!(refusal, "the validators' rejection: {rejected:?}");
    let pushes: Vec<usize> = validators.iter().map(|v| v.calls("sendrawtransaction")).collect();
    assert_eq!(pushes, [1, 1, 1], "pushes per entry: each once");
    cancel.cancel();
}

/// No `checkadmission` anywhere, every `sendrawtransaction` unreachable
#[tokio::test(start_paused = true)]
async fn an_unchecked_send_no_entry_answers_is_unreachable_after_trying_each_never_a_rejection() {
    let mut chain = MockChain::regtest();
    let best = chain.mine_empty(50);
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(best))).collect();
    for validator in &validators {
        validator.lacks(&["checkadmission"]);
        validator.reachable(&["sendrawtransaction"], false);
    }
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, best)));

    let validated = view.validate_transaction(raw_transaction(6, 0).1).await.expect("none judges");
    let sent = view.broadcast_transaction(validated).await;

    let unreachable = matches!(sent, Err(SubmitError::Unreachable { attempted: 3, .. }));
    assert!(unreachable, "3 attempts, nothing learnt about the transaction: {sent:?}");
    cancel.cancel();
}

/// Every validator accepting, none gossiping on its own; no header chain = nothing to check with
#[tokio::test(start_paused = true)]
async fn an_unspread_send_is_resubmitted_past_the_threshold_until_an_outsider_lists_it() {
    let mut chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    let policy = SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(4).expect("nz"),
    };
    let view = view.with_submit_policy(policy);
    next_poll_merged(&balancer).await;
    let pushes =
        || -> Vec<usize> { validators.iter().map(|v| v.calls("sendrawtransaction")).collect() };
    let (txid, sent) = raw_transaction(5, 0);

    let validated = view.validate_transaction(sent.clone()).await.expect("valid");
    view.broadcast_transaction(validated).await.expect("the first entry accepts");
    let sent_at = Instant::now();
    let first = pushes();
    tokio::time::sleep_until(sent_at + policy.propagation_threshold - POLL).await;
    let inside = pushes();
    tokio::time::sleep_until(sent_at + policy.propagation_threshold + POLL).await;
    let second = pushes();
    let outsider = second.iter().position(|&n| n == 0).expect("one entry untried");
    chain.submit_raw(sent);
    validators[outsider].hear(&chain, txid);
    next_poll_merged(&balancer).await;
    tokio::time::sleep(policy.propagation_threshold * u32::from(policy.max_attempts.get())).await;

    assert_eq!(first.iter().sum::<usize>(), 1, "one entry first: {first:?}");
    assert_eq!(inside, first, "inside the threshold: no resubmit");
    assert_eq!(second.iter().sum::<usize>(), 2, "past it, unspread: a second entry: {second:?}");
    assert!(second.iter().all(|&n| n <= 1), "a different validator: {second:?}");
    assert_eq!(pushes(), second, "an outsider lists it: spread, nothing more pushed");
    let listers = view.subscriber().current().sighting(&txid).map(|s| s.trusted().count());
    assert_eq!(listers, Some(3), "trusted listers: both entries + the outsider");
    cancel.cancel();
}

/// Who holds a transaction in the view: fresh peer listers, trusted listers, mempools read
#[derive(Debug, PartialEq, Eq)]
struct Spread {
    peer_listers: usize,
    trusted_listers: usize,
    trusted_read: usize,
}

/// Two trusted validators, four live peers in four netgroups (no quorum: > 5 needed) + one dead
#[tokio::test(start_paused = true)]
async fn a_peer_listing_without_a_quorum_holds_nothing_until_a_trusted_validator_lists_it() {
    let mut chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..2).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    let (view, balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
    let view = view.with_peers(peers.clone());
    let polling = view.peer_mempool(Arc::new(chain.ledger(chain.genesis()))).expect("peers");
    tokio::spawn(polling.run(cancel.clone()));
    next_poll_merged(&balancer).await;
    let gossiped = chain.submit_raw(raw_transaction(1, 0).1);
    peers.list(peer(1), vec![peer_tx_id(gossiped)]);
    peers.list(peer(2), vec![peer_tx_id(gossiped)]);

    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    let peers_only = view.subscriber().current();
    validators[0].hear(&chain, gossiped);
    next_poll_merged(&balancer).await;
    let listed = view.subscriber().current();

    assert_eq!(peers_only.sighting(&gossiped), None, "2 of 4 peers, 4 netgroups: not held");
    let sighting = listed.sighting(&gossiped).expect("held once a trusted validator lists it");
    let spread = Spread {
        peer_listers: listed.listings.listers(&gossiped).len(),
        trusted_listers: sighting.trusted().count(),
        trusted_read: listed.mempool_readers().count(),
    };
    let expected = Spread { peer_listers: 2, trusted_listers: 1, trusted_read: 2 };
    assert_eq!(spread, expected, "a's listing holds it, the peers' still counted");
    cancel.cancel();
}

/// Two trusted holders, four live peers in four netgroups (no quorum) + one dead
#[tokio::test(start_paused = true)]
async fn a_checked_send_with_peers_relays_through_them_only_again_past_the_threshold_until_listed()
{
    let mut chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..2).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    let (view, balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
    let policy = SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = view.with_submit_policy(policy).with_peers(peers.clone());
    let polling = view.peer_mempool(Arc::new(chain.ledger(chain.genesis()))).expect("peers");
    tokio::spawn(polling.run(cancel.clone()));
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, chain.genesis())));
    next_poll_merged(&balancer).await;
    let (txid, sent) = raw_transaction(2, 0);

    let validated = view.validate_transaction(sent.clone()).await.expect("a holder admits it");
    let answered = view.broadcast_transaction(validated).await;
    let sent_at = Instant::now();
    tokio::task::yield_now().await;
    let first = peers.pushes();
    tokio::time::sleep_until(sent_at + policy.propagation_threshold - POLL).await;
    let inside = peers.pushes();
    tokio::time::sleep_until(sent_at + policy.propagation_threshold + POLL).await;
    let again = peers.pushes();
    peers.list(peer(3), vec![peer_tx_id(txid)]);
    chain.submit_raw(sent);
    validators[1].hear(&chain, txid);
    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    next_poll_merged(&balancer).await;
    let listed = view.subscriber().current();
    tokio::time::sleep(policy.propagation_threshold * u32::from(policy.max_attempts.get())).await;

    assert_eq!(answered.expect("answered on the check"), txid, "the txid of the bytes");
    let calls = |method| validators.iter().map(|v| v.calls(method)).sum::<usize>();
    assert_eq!(calls("checkadmission"), 1, "one check");
    assert_eq!(
        [first, inside, again],
        [1, 1, 2],
        "peer relays: at once, inside, past the threshold"
    );
    let sighting = listed.sighting(&txid).expect("held");
    assert!(sighting.ours(), "ours");
    let spread = Spread {
        peer_listers: listed.listings.listers(&txid).len(),
        trusted_listers: sighting.trusted().count(),
        trusted_read: listed.mempool_readers().count(),
    };
    assert_eq!(
        spread,
        Spread { peer_listers: 1, trusted_listers: 1, trusted_read: 2 },
        "b lists it"
    );
    assert_eq!(peers.pushes(), 2, "a trusted validator never pushed to lists it: nothing more");
    assert_eq!(calls("sendrawtransaction"), 0, "never pushed to a trusted validator");
    cancel.cancel();
}

/// No `checkadmission`, peers relaying nowhere: `max_attempts` relays, each waiting its threshold
#[tokio::test(start_paused = true)]
async fn an_unchecked_send_with_peers_spends_its_budget_on_peer_relays_then_one_trusted_verdict() {
    let chain = MockChain::regtest();
    let validators: Vec<Arc<MockValidator>> =
        (0..2).map(|_| Arc::new(chain.validator(chain.genesis()))).collect();
    for validator in &validators {
        validator.lacks(&["checkadmission"]);
    }
    let (view, balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
    let policy = SubmitPolicy {
        propagation_threshold: Duration::from_secs(10),
        max_attempts: std::num::NonZeroU8::new(3).expect("nz"),
    };
    let view = Arc::new(view.with_submit_policy(policy).with_peers(peers.clone()));
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, chain.genesis())));
    next_poll_merged(&balancer).await;
    let (txid, sent) = raw_transaction(3, 0);
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move {
        let validated = submitting.validate_transaction(sent).await?;
        submitting.broadcast_transaction(validated).await
    });

    let budget = policy.propagation_threshold * u32::from(policy.max_attempts.get());
    tokio::time::sleep(budget + POLL).await;
    let answered = within("the wallet answered", answer).await.expect("submission task");

    assert_eq!(answered.expect("the trusted verdict accepts"), txid, "the txid of the bytes");
    assert_eq!(peers.pushes(), 3, "max_attempts relays through the peers first");
    let pushes: usize = validators.iter().map(|v| v.calls("sendrawtransaction")).sum();
    assert_eq!(pushes, 1, "then exactly one trusted verdict");
    cancel.cancel();
}

/// Trusted validator + seven live peers in seven netgroups, all listing `a`, then the validator
/// down
#[tokio::test(start_paused = true)]
async fn a_listing_handed_to_a_peer_quorum_keeps_its_sighting_when_the_validator_goes_down() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let block = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 10_000)));
    let funding = chain.output(block, 0, 0);
    let tip = chain.mine_empty(2);
    let validator = Arc::new(chain.validator(tip));
    let (view, balancer, cancel) = view_merging_polls(&[Arc::clone(&validator)], &["a:8232"], None);
    let seven: Vec<SocketAddr> = (1..=7).map(peer).collect();
    let peers = Arc::new(MockPeers::new(seven.clone(), []));
    let view = view.with_peers(peers.clone());
    tokio::spawn(
        view.peer_mempool(Arc::new(chain.ledger(tip))).expect("peers").run(cancel.clone()),
    );
    let (a, a_raw) = raw_transparent(&[funding], &[(&alice, 9_000)]);
    peers.hold(peer_tx_id(a), Bytes::from(a_raw.clone()));
    seven.iter().for_each(|peer| peers.list(*peer, vec![peer_tx_id(a)]));
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
    chain.submit_raw(a_raw);
    validator.hear(&chain, a);
    let reader = view.subscriber();
    // a sighting's first render survives it (a re-added one renders afresh)
    let rendered = |text: &'static str| {
        let render = || Ok::<_, ()>(Bytes::from_static(text.as_bytes()));
        reader.current().sighting(&a).map(|s| s.entry(a).projection.get_or_render(render))
    };
    next_poll_merged(&balancer).await;
    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    let listed = rendered("listed");

    validator.down(true);
    down_and_merged(&balancer, 0).await;

    let pinned = reader.current();
    assert_eq!(listed, Some(Ok(Bytes::from("listed"))), "held by the validator + the quorum");
    assert_eq!(pinned.trusted_validators()[0].health, Health::Down, "its listing retracted");
    assert!(
        pinned.held_by().is_empty(),
        "no trusted validator holds the tip: {:?}",
        pinned.held_by()
    );
    let served: Vec<(TxId, Option<Zatoshis>)> =
        pinned.mempool().expect("a quorum reads").entries().map(|e| (e.txid, e.fee)).collect();
    assert_eq!(served, [(a, Some(zats(1_000)))], "the quorum keeps it, at the validator's fee");
    assert_eq!(rendered("re-added"), listed, "the same sighting throughout");
    cancel.cancel();
}

/// Trusted validator down from the start; seven peers in seven netgroups list `parent`, `child`
/// (spends `parent`'s output) and `bad` (pays 6,000 of 5,000)
#[tokio::test(start_paused = true)]
async fn a_peer_quorum_admits_a_child_a_round_after_its_parent_and_refuses_a_negative_fee_once() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let [parent_funding, bad_funding] = [10_000, 5_000].map(|zats| {
        let block = chain.mine(|b| b.coinbase(|c| c.pay(&alice, zats)));
        chain.output(block, 0, 0)
    });
    let tip = chain.mine_empty(2);
    let validator = Arc::new(chain.validator(tip));
    validator.down(true);
    let (view, _balancer, cancel) = view_merging_polls(&[validator], &["a:8232"], None);
    let seven: Vec<SocketAddr> = (1..=7).map(peer).collect();
    let peers = Arc::new(MockPeers::new(seven.clone(), []));
    let view = view.with_peers(peers.clone());
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
    tokio::spawn(
        view.peer_mempool(Arc::new(chain.ledger(tip))).expect("peers").run(cancel.clone()),
    );
    let (parent, parent_raw) = raw_transparent(&[parent_funding], &[(&alice, 9_000)]);
    let (child, child_raw) =
        raw_transparent(&[OutPoint::new(parent.into(), 0)], &[(&alice, 8_500)]);
    let (bad, bad_raw) = raw_transparent(&[bad_funding], &[(&alice, 6_000)]);
    for (txid, raw) in [(parent, parent_raw), (child, child_raw), (bad, bad_raw)] {
        peers.hold(peer_tx_id(txid), Bytes::from(raw));
    }
    let listed = vec![peer_tx_id(parent), peer_tx_id(child), peer_tx_id(bad)];
    seven.iter().for_each(|peer| peers.list(*peer, listed.clone()));
    let reader = view.subscriber();
    let served = || -> Vec<(TxId, Option<Zatoshis>)> {
        let view = reader.current();
        let mut served: Vec<_> =
            view.mempool().expect("a quorum reads").entries().map(|e| (e.txid, e.fee)).collect();
        served.sort();
        served
    };

    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    let first_round = served();
    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    let second_round = served();
    tokio::time::sleep(PEER_MEMPOOL_POLL * 2).await;

    assert_eq!(
        first_round,
        [(parent, Some(zats(1_000)))],
        "child waits on its parent, bad refused"
    );
    let mut both = vec![(parent, Some(zats(1_000))), (child, Some(zats(500)))];
    both.sort();
    assert_eq!(
        second_round, both,
        "child retried (unknown prevout), admitted once its parent is held"
    );
    let bad_fetches = peers.fetched().into_iter().filter(|fetched| fetched.txid == bad).count();
    assert_eq!(bad_fetches, 1, "bad refused once, never re-fetched while the quorum lists it");
    cancel.cancel();
}

/// Trusted validator down from the start, seven peers in seven netgroups; 6 of 7 list the send
#[tokio::test(start_paused = true)]
async fn with_no_trusted_validator_a_send_relays_once_via_peers_answered_once_a_quorum_lists_it() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let block = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 10_000)));
    let funding = chain.output(block, 0, 0);
    let tip = chain.mine_empty(2);
    let validator = Arc::new(chain.validator(tip));
    validator.down(true);
    let (view, _balancer, cancel) =
        view_merging_polls(&[Arc::clone(&validator)], &["a:8232"], None);
    let seven: Vec<SocketAddr> = (1..=7).map(peer).collect();
    let peers = Arc::new(MockPeers::new(seven.clone(), []));
    let view = Arc::new(view.with_peers(peers.clone()));
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
    tokio::spawn(
        view.peer_mempool(Arc::new(chain.ledger(tip))).expect("peers").run(cancel.clone()),
    );
    let (sent, sent_raw) = raw_transparent(&[funding], &[(&alice, 9_500)]);
    peers.hold(peer_tx_id(sent), Bytes::from(sent_raw.clone()));
    let submitting = Arc::clone(&view);
    let answer = tokio::spawn(async move {
        let validated = submitting.validate_transaction(sent_raw).await?;
        submitting.broadcast_transaction(validated).await
    });

    tokio::task::yield_now().await;
    let relayed = peers.pushes();
    seven[1..].iter().for_each(|peer| peers.list(*peer, vec![peer_tx_id(sent)]));
    tokio::time::sleep(PEER_MEMPOOL_POLL).await;
    let answered = within("the wallet answered", answer).await.expect("submission task");

    assert_eq!(relayed, 1, "no holder to check with: one peer relay");
    let trusted = ["sendrawtransaction", "checkadmission"].map(|method| validator.calls(method));
    assert_eq!(
        trusted,
        [0, 0],
        "trusted validator never asked (sendrawtransaction, checkadmission)"
    );
    assert_eq!(answered.expect("a peer quorum admitted it"), sent, "the txid of the bytes");
    let served: Vec<(TxId, Option<Zatoshis>)> = view
        .subscriber()
        .current()
        .mempool()
        .expect("a quorum reads")
        .entries()
        .map(|e| (e.txid, e.fee))
        .collect();
    assert_eq!(served, [(sent, Some(zats(500)))], "ours, priced from the UTXO set");
    cancel.cancel();
}

/// a, b, c at 100 (varied work); a mines 101, a new transaction's bytes 20 s away on a
#[tokio::test(start_paused = true)]
async fn a_new_verified_block_moves_the_tip_at_once_held_by_its_one_holder_before_slow_bytes() {
    let mut chain = MockChain::regtest().varied_work();
    chain.mine_empty(101);
    let (at_100, at_101) = (chain.at(h(100)), chain.at(h(101)));
    let validators: Vec<Arc<MockValidator>> =
        (0..3).map(|_| Arc::new(chain.validator(at_100))).collect();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at_100)));
    validators[0].follow(&chain, at_101);
    validators[0].latency(&["getrawtransaction"], Duration::from_secs(20));
    let unmined = chain.submit_raw(raw_transaction(1, 0).1);
    validators[0].hear(&chain, unmined);

    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at_101)));

    let pinned = view.subscriber().current();
    assert_eq!(pinned.best(), Some(at_101), "the verified 101");
    assert_eq!(pinned.held_by(), TrustedValidatorSet::at([0]), "one holder, its bytes 20 s away");
    let agreements: Vec<Agreement> = pinned.agreements().collect();
    let expected = [Agreement::Agreed, Agreement::Behind, Agreement::Behind];
    assert_eq!(agreements, expected, "a at 101, b and c at 100");
    let links: usize = validators.iter().map(|v| v.calls("getblockheader")).sum();
    assert_eq!(links, 0, "holding asked by getblockhash in the poll: no header reads");
    cancel.cancel();
}

/// a alone at 101 (b, c at 100), then a unreachable
#[tokio::test(start_paused = true)]
async fn with_its_only_holder_failing_the_tip_has_no_holder_and_no_mempool_until_one_returns() {
    let mut chain = MockChain::regtest().varied_work();
    chain.mine_empty(101);
    let (at_100, at_101) = (chain.at(h(100)), chain.at(h(101)));
    let validators: Vec<Arc<MockValidator>> =
        [at_101, at_100, at_100].map(|tip| Arc::new(chain.validator(tip))).to_vec();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, at_101)));
    next_poll_merged(&balancer).await;
    let reader = view.subscriber();

    validators[0].down(true);
    next_poll_merged(&balancer).await;
    let lost = reader.current();
    validators[1].follow(&chain, at_101);
    next_poll_merged(&balancer).await;
    let caught_up = reader.current().held_by();
    validators[0].down(false);
    next_poll_merged(&balancer).await;
    let returned = reader.current().held_by();

    let health = lost.trusted_validators()[0].health;
    assert_eq!(health, Health::Degraded, "a: one failure = degraded, out of the holders");
    assert!(lost.held_by().is_empty(), "no holder of 101: {:?}", lost.held_by());
    assert!(lost.mempool().is_none(), "no holder: no mempool (nothing proves 101 valid)");
    assert_eq!(lost.best(), Some(at_101), "the header chain still says 101");
    assert_eq!(caught_up, TrustedValidatorSet::at([1]), "b catches up: holds 101");
    assert_eq!(returned, TrustedValidatorSet::at([0, 1]), "a answers again: holds it too");
    cancel.cancel();
}

/// a, b at 101 on the trunk; c on a heavier fork from 100
#[tokio::test(start_paused = true)]
async fn a_heavier_fork_one_validator_holds_is_the_tip_held_by_it_alone() {
    let mut chain = MockChain::regtest().varied_work();
    let at_101 = chain.mine_empty(101);
    let fork = chain.fork(h(100)).outweigh().mine_empty(1).tip();
    let validators: Vec<Arc<MockValidator>> =
        [at_101, at_101, fork].map(|tip| Arc::new(chain.validator(tip))).to_vec();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);

    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, fork)));

    let pinned = view.subscriber().current();
    assert_eq!(pinned.best(), Some(fork), "the tip follows the work");
    assert_eq!(pinned.held_by(), TrustedValidatorSet::at([2]), "c alone holds the fork");
    let agreements: Vec<Agreement> = pinned.agreements().collect();
    let expected = [Agreement::Diverged, Agreement::Diverged, Agreement::Agreed];
    assert_eq!(agreements, expected, "a, b on the lighter trunk");
    cancel.cancel();
}

/// c on a heavier fork from 100 (the tip), then back onto the trunk between its tip read and its
/// `getblockhash` answer
#[tokio::test(start_paused = true)]
async fn a_validator_reorging_mid_poll_holds_the_fork_for_that_poll_and_nothing_after() {
    let mut chain = MockChain::regtest().varied_work();
    let at_101 = chain.mine_empty(101);
    let fork = chain.fork(h(100)).outweigh().mine_empty(1).tip();
    let validators: Vec<Arc<MockValidator>> =
        [at_101, at_101, fork].map(|tip| Arc::new(chain.validator(tip))).to_vec();
    let (view, balancer, cancel) =
        view_merging_polls(&validators, &["a:8232", "b:8232", "c:8232"], None);
    next_poll_merged(&balancer).await;
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, fork)));
    let reader = view.subscriber();
    let mut c = balancer.observe(v(2));
    c.borrow_and_update();

    validators[2].reorg_after_next_poll(&chain, at_101);
    within("c's raced poll", c.changed()).await.expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let raced = c.borrow().as_ref().map(|observation| observation.polled.is_ok());
    let raced_holders = reader.current().held_by();
    within("c's next poll", c.changed()).await.expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;

    assert_eq!(raced, Some(true), "a race is not a failure");
    assert_eq!(raced_holders, TrustedValidatorSet::at([2]), "the raced poll: its claim, as read");
    let reasked = reader.current();
    assert!(reasked.held_by().is_empty(), "re-asked on the trunk: nothing of the fork held");
    assert_eq!(reasked.trusted_validators()[2].tip(), Some(at_101), "c's tip: back at 101");
    assert_eq!(reasked.agreements().nth(2), Some(Agreement::Diverged), "c off the fork");
    cancel.cancel();
}

/// one and two, each its own peers; one's metadata read then fails, then answers anew
#[tokio::test(start_paused = true)]
async fn metadata_rides_one_poll_a_minute_keyed_by_address_and_a_failed_read_keeps_the_last() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(10);
    let [one, two] = [(); 2].map(|_| Arc::new(chain.validator(tip)));
    let inbound = PeerInfo { addr: "x:1".to_owned(), inbound: true };
    one.telemetry(Telemetry { peers: Some(vec![outbound("seed-a:8233"), inbound]), release: None });
    two.telemetry(Telemetry { peers: Some(vec![outbound("seed-z:8233")]), release: None });
    let (view, balancer, cancel) =
        view_merging_polls(&[Arc::clone(&one), two], &["one:8232", "two:8232"], None);
    let reader = view.subscriber();
    let peers = || -> Vec<(String, Vec<PeerInfo>)> {
        let pinned = reader.current();
        let each = pinned.trusted_validators().iter();
        each.map(|meta| (meta.address.clone(), meta.telemetry.peers.clone().unwrap_or_default()))
            .collect()
    };

    next_poll_merged(&balancer).await;
    let first = peers();
    one.telemetry(Telemetry::default());
    tokio::time::sleep(METADATA_EVERY).await;
    let failed = (peers(), reader.current().trusted_validators()[0].health);
    one.telemetry(Telemetry { peers: Some(Vec::new()), release: None });
    next_poll_merged(&balancer).await;
    let unrefreshed = peers();
    tokio::time::sleep(METADATA_EVERY).await;
    let refreshed = peers();

    let inbound = PeerInfo { addr: "x:1".to_owned(), inbound: true };
    let one_peers = vec![outbound("seed-a:8233"), inbound];
    let expected = vec![
        ("one:8232".to_owned(), one_peers),
        ("two:8232".to_owned(), vec![outbound("seed-z:8233")]),
    ];
    assert_eq!(first, expected, "each validator's peers, keyed by its configured address");
    assert_eq!(failed.0, first, "one's read failed: its last answer kept");
    assert_eq!(failed.1, Health::Live, "a metadata failure is not a poll failure (else Degraded)");
    assert_eq!(unrefreshed, first, "read once per refresh, not per poll");
    assert_eq!(refreshed[0].1, [], "a fresh answer replaces it (isolated = empty, not an error)");
    cancel.cancel();
}

/// Release halting 4,960 blocks past its tip (inside a week), then an upgrade's far later
#[tokio::test(start_paused = true)]
async fn a_release_halting_within_a_week_raises_ending_and_an_upgrade_clears_it() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(10);
    let validator = Arc::new(chain.validator(tip));
    let release = |build: &str, halts: u32| NodeRelease {
        build: build.to_owned(),
        user_agent: format!("/Zebra:{}/", &build[1..]),
        end_of_service: EndOfService::At { height: h(halts) },
    };
    validator.telemetry(Telemetry { peers: None, release: Some(release("v6.4.2", 10 + 4_960)) });
    let (view, balancer, cancel) =
        view_merging_polls(&[Arc::clone(&validator)], &["one:8232"], None);
    let reader = view.subscriber();

    next_poll_merged(&balancer).await;
    let ending = reader.current();
    validator.telemetry(Telemetry { peers: None, release: Some(release("v6.5.0", 3_700_000)) });
    tokio::time::sleep(METADATA_EVERY).await;
    let upgraded = reader.current();

    let meta = &ending.trusted_validators()[0];
    assert_eq!(meta.blocks_to_end_of_service(), Some(4_960), "halts 4,960 blocks past its tip");
    assert_eq!(ending.alarms().by(Alarm::Ending), TrustedValidatorSet::at([0]), "ending raised");
    let build =
        upgraded.trusted_validators()[0].telemetry.release.as_ref().map(|r| r.build.clone());
    assert_eq!(build.as_deref(), Some("v6.5.0"), "the upgrade read at the next refresh");
    assert!(!upgraded.alarms().raised(Alarm::Ending), "upgraded: {:?}", upgraded.alarms());
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn the_push_streams_state_shows_from_the_next_poll() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(10);
    let validator = Arc::new(chain.validator(tip));
    let (view, balancer, cancel) = view_merging_polls(&[validator], &["one:8232"], None);
    let streaming = || view.subscriber().current().trusted_validators()[0].streaming;
    next_poll_merged(&balancer).await;
    let before = streaming();

    balancer.pushed(v(0), Push::Link(true));
    next_poll_merged(&balancer).await;
    let up = streaming();
    balancer.pushed(v(0), Push::Link(false));
    next_poll_merged(&balancer).await;
    let down = streaming();

    assert_eq!([before, up, down], [false, true, false], "streaming: before, after up, after down");
    cancel.cancel();
}

/// Fail closed, never stale: a `Down` validator's holds + sightings withdrawn, still probed
#[tokio::test(start_paused = true)]
async fn a_validator_gone_down_withdraws_its_hold_and_listings_and_its_return_restores_both() {
    let mut chain = MockChain::regtest();
    let genesis = chain.genesis();
    let validator = Arc::new(chain.validator(genesis));
    let tx1 = chain.submit_raw(raw_transaction(1, 0).1);
    validator.hear(&chain, tx1);
    let (view, balancer, cancel) =
        view_merging_polls(&[Arc::clone(&validator)], &["one:8232"], None);
    view.set_chain(Some(HeaderChainSnapshot::of(&chain, genesis)));
    let reader = view.subscriber();
    let serves_tx1 = || {
        let pinned = reader.current();
        pinned.mempool().is_some_and(|mempool| mempool.entries().any(|entry| entry.txid == tx1))
    };
    eventually("tx 1 served", serves_tx1).await;

    validator.down(true);
    down_and_merged(&balancer, 0).await;
    let gone = reader.current();
    validator.down(false);
    eventually("tx 1 served again", serves_tx1).await;

    assert_eq!(gone.trusted_validators()[0].health, Health::Down, "down past the ladder");
    assert!(gone.held_by().is_empty(), "no holder left: {:?}", gone.held_by());
    assert!(gone.mempool().is_none(), "fail closed: no mempool");
    assert_eq!(gone.sighting(&tx1).map(|s| s.trusted()), None, "its listing retracted: tx 1 gone");
    assert!(gone.mempool_readers().is_empty(), "none left reading: {:?}", gone.mempool_readers());
    assert_eq!(reader.current().trusted_validators()[0].health, Health::Live, "back: live");
    cancel.cancel();
}

/// Depth 10: best 30 finalized at 20 owes nothing; best 45 finalized at 20 owes 15
#[tokio::test(start_paused = true)]
async fn finality_paused_rises_after_finality_stall_unmoved_and_clears_when_finalized_moves() {
    let mut chain = MockChain::regtest();
    let tip_30 = chain.mine_empty(30);
    let tip_45 = chain.mine_empty(15);
    let validator = Arc::new(chain.validator(tip_45));
    let (view, _balancer, cancel) = view_merging_polls(&[validator], &["a:8232"], None);
    let paused = || view.subscriber().current().alarms().raised(Alarm::FinalityPaused);

    view.set_chain(Some(HeaderChainSnapshot::finalized(&chain, tip_30, h(20))));
    tokio::time::sleep(FINALITY_STALL + POLL).await;
    let owed_nothing = paused();
    view.set_chain(Some(HeaderChainSnapshot::finalized(&chain, tip_45, h(20))));
    tokio::time::sleep(FINALITY_STALL - POLL).await;
    let not_yet = paused();
    tokio::time::sleep(POLL * 2).await;
    let stalled = paused();
    view.set_chain(Some(HeaderChainSnapshot::finalized(&chain, tip_45, h(35))));
    let moved = paused();

    assert!(!owed_nothing, "finalized = best − depth for {FINALITY_STALL:?}+: nothing owed");
    assert!(!not_yet, "owed, unmoved just short of {FINALITY_STALL:?}: not yet");
    assert!(stalled, "owed, unmoved past {FINALITY_STALL:?}: raised");
    assert!(!moved, "finalized moved: cleared at once");
    cancel.cancel();
}

/// Real regtest header bytes through header sync: a at 5,000, b at 3,000 (below a's finalized)
#[tokio::test(start_paused = true)]
async fn header_sync_anchors_context_below_a_trusted_claim_and_reads_no_history_below_it() {
    let mut chain = MockChain::regtest();
    let a = chain.mine_empty(5_000);
    let at = |height: u32| chain.at(h(height));
    let below_claim = u32::try_from(CONTEXT).expect("CONTEXT fits a height");
    let (b, anchor, finalized) = (at(3_000), at(5_000 - below_claim), at(5_000 - DEPTH));
    let validators = [a, b].map(|tip| Arc::new(chain.validator(tip)));
    let (view, _balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let sync = view.header_sync(HeaderChain::new(depth()));
    let chains = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || chains.borrow().clone().map(|c| (c.best(), c.finalized_tip()));

    eventually("a's tip followed", || published() == Some((a, finalized))).await;

    let first = chains.borrow().clone().expect("a HeaderChainSnapshot");
    let pinned = view.subscriber().current();
    assert_eq!(pinned.held_by(), TrustedValidatorSet::at([0]), "a holds the tip, b behind it");
    assert_eq!(first.hash_at(anchor.height), Some(anchor.hash), "anchored at 5,000 − CONTEXT");
    assert_eq!(first.hash_at(anchor.height - 1), None, "no history below the anchor");
    assert!(first.on_best(b), "b's claim below finalized: finalized by definition");
    let fetched: Vec<usize> = validators.iter().map(|v| v.calls("getblockheader")).collect();
    assert_eq!(fetched, [1 + CONTEXT, 0], "headers read (a, b): the anchor + CONTEXT");
    assert!(!pinned.alarms().raised(Alarm::FinalityPaused), "{:?}", pinned.alarms());
    cancel.cancel();
}

/// a, b at 5,000; b moves to a fork of it at 4,998 with more work
#[tokio::test(start_paused = true)]
async fn the_tip_follows_the_work_to_a_heavier_fork_and_a_published_chain_never_changes() {
    let mut chain = MockChain::regtest();
    let a = chain.mine_empty(5_000);
    let heavier = chain.branch(chain.at(h(4_998))).mine_empty(22).tip();
    let validators = [a, a].map(|tip| Arc::new(chain.validator(tip)));
    let (view, balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let chains = sync.subscribe();
    let syncing = tokio::spawn(sync.run(cancel.clone()));
    let published = || chains.borrow().clone().map(|c| c.best());
    eventually("a's tip followed", || published() == Some(a)).await;
    let first = chains.borrow().clone().expect("a HeaderChainSnapshot");

    validators[1].follow(&chain, heavier);
    next_poll_merged(&balancer).await;
    eventually("b's heavier fork followed", || reader.current().best() == Some(heavier)).await;
    eventually("b's chain published", || published() == Some(heavier)).await;

    let held = reader.current().held_by();
    assert_eq!(held, TrustedValidatorSet::at([1]), "the work, not the first: b holds it");
    assert_eq!(first.best(), a, "a published chain never changes (H5)");
    cancel.cancel();
    within("header sync ends on cancel", syncing).await.expect("header sync never panics");
}

/// a far ahead, a poll forced after every publish: the chain re-anchors at its claim − depth
///
/// - a at 20: anchored at genesis (20 − max(depth, CONTEXT), saturated), followed, finalized 10
/// - a jumps to 20,500 (past depth + `HEADER_BATCH` above finalized) → re-anchored at
///   20,500 − CONTEXT, followed, finalized 20,490: none of the 20,366 between read
/// - every published chain: best − finalized ≤ depth + `HEADER_BATCH` (H9), finality alarm down
#[tokio::test(start_paused = true)]
async fn a_validator_far_ahead_re_anchors_the_chain_instead_of_syncing_toward_it() {
    let mut chain = MockChain::regtest();
    let at_20 = chain.mine_empty(20);
    let tip = chain.mine_empty(20_480);
    let (genesis, at_10) = (chain.genesis(), chain.at(h(10)));
    let below_claim = u32::try_from(CONTEXT).expect("CONTEXT fits a height");
    let (re_anchor, finalized) = (chain.at(h(20_500 - below_claim)), chain.at(h(20_500 - DEPTH)));
    let validator = Arc::new(chain.validator(at_20));
    let (view, balancer, cancel) = view_merging_polls(&[Arc::clone(&validator)], &["a:8232"], None);
    let reader = view.subscriber();
    let sync = view.header_sync(HeaderChain::new(depth()));
    let mut verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));

    let mut followed = Vec::new();
    loop {
        within("header sync progresses", verified.changed()).await.expect("header sync running");
        let (best, finalized_tip) = {
            let published = verified.borrow_and_update();
            let published = published.as_ref().expect("a chain once published");
            (published.best(), published.finalized_tip())
        };
        let non_finalized = u32::from(best.height) - u32::from(finalized_tip.height);
        let bound = DEPTH + HEADER_BATCH;
        assert!(non_finalized <= bound, "{best:?}: {non_finalized} non-finalized > {bound} (H9)");
        let pinned = reader.current();
        let alarms = pinned.alarms();
        assert!(!alarms.raised(Alarm::FinalityPaused), "{best:?}: {alarms:?}");
        followed.push(finalized_tip);
        balancer.pushed(v(0), Push::Changed);
        if (best, finalized_tip) == (at_20, at_10) {
            validator.follow(&chain, tip);
        }
        if (best, finalized_tip) == (tip, finalized) {
            break;
        }
    }

    let anchors_and_finalized = [genesis, at_10, re_anchor, finalized];
    let jumped = followed.iter().all(|finalized_tip| anchors_and_finalized.contains(finalized_tip));
    assert!(jumped, "finalized tip = an anchor or its run's end only: {followed:?}");
    let links = validator.calls("getblockheader");
    assert_eq!(links, 1 + 20 + 1 + CONTEXT, "two anchors + their runs, nothing between");
    eventually("the view's tip, held", || {
        reader.current().held_by() == TrustedValidatorSet::at([0])
    })
    .await;
    cancel.cancel();
}

/// a, b at 30 (depth 10); a then serves a longer chain forked at 5, below our finalized 20
#[tokio::test(start_paused = true)]
async fn a_forked_validator_never_delays_the_honest_one() {
    /// Poll + its header fetch
    const FOLLOWED_WITHIN: Duration = Duration::from_millis(1_500);
    /// Six stall back-offs: a forked claim fetched again would show here
    const REFETCH_WINDOW: Duration = Duration::from_secs(30);
    let mut chain = MockChain::regtest();
    let at_30 = chain.mine_empty(30);
    let at_31 = chain.mine_empty(1);
    let at_32 = chain.mine_empty(1);
    let (at_20, at_21, at_22) = (chain.at(h(20)), chain.at(h(21)), chain.at(h(22)));
    let forked = chain.fork(h(5)).mine_empty(60).tip();
    let validators = [at_30, at_30].map(|tip| Arc::new(chain.validator(tip)));
    let (view, _balancer, cancel) = view_merging_polls(&validators, &["a:8232", "b:8232"], None);
    let sync = view.header_sync(HeaderChain::new(depth()));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));
    let followed = async |tip: BlockRef, finalized_tip: BlockRef| {
        let asked = Instant::now();
        eventually("followed", || published() == Some((tip, finalized_tip))).await;
        asked.elapsed()
    };
    followed(at_30, at_20).await;

    validators[0].follow(&chain, forked);
    validators[1].follow(&chain, at_31);
    let took_31 = followed(at_31, at_21).await;
    tokio::time::sleep(REFETCH_WINDOW).await;
    let links = validators[0].calls("getblockheader");
    validators[1].follow(&chain, at_32);
    let took_32 = followed(at_32, at_22).await;
    tokio::time::sleep(REFETCH_WINDOW).await;

    assert!(took_31 <= FOLLOWED_WITHIN, "b's 31 after {took_31:?}, a forked beside it");
    assert!(took_32 <= FOLLOWED_WITHIN, "b's 32 after {took_32:?}, a forked beside it");
    assert_eq!(validators[0].calls("getblockheader"), links, "a's forked claim never read again");
    let pinned = view.subscriber().current();
    assert_eq!(pinned.best(), Some(at_32), "b's chain");
    assert_eq!(pinned.agreements().next(), Some(Agreement::Diverged), "a: forked below finalized");
    cancel.cancel();
}

/// a + the peer set at 40 (depth 10), real regtest headers; a then down, the network on to 60
///
/// - peers 10.1–10.5 (two in 10.5) announce 60: five netgroups, finalized unmoved
/// - none counts: 10.7 announces a block off our chain, 10.8 (not live: inbound) announces 60
/// - 10.6 announces 45, then 55: six netgroups carried each → finalized 45, then 50 (best − depth)
#[tokio::test(start_paused = true)]
async fn past_a_down_validator_peers_extend_the_tip_and_six_netgroups_carrying_it_finalize_it() {
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
        view_merging_polls(&[Arc::clone(&validator)], &["a:8232"], Some(transport));
    let at = |b: u8, host: u8| SocketAddr::from(([10, b, 0, host], 8233));
    let live = [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2), (6, 1), (7, 1)];
    let announcing = Arc::new(MockPeers::new(live.map(|(b, host)| at(b, host)), []));
    let view = view.with_peers(announcing.clone());
    tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
    let sync = view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(regtest()));
    let verified = sync.subscribe();
    tokio::spawn(sync.run(cancel.clone()));
    let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));
    let held_by = || view.subscriber().current().held_by();
    eventually("anchored, a followed", || published() == Some((at_40, at_30))).await;

    validator.down(true);
    peer_set.hold(h(60));
    eventually("the peers' tip, unvouched", || published() == Some((at_60, at_40))).await;
    eventually("best held by no one", || held_by().is_empty()).await;
    for (b, host) in [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (5, 2)] {
        announcing.announce_block(at(b, host), at_60.hash);
    }
    announcing.announce_block(at(7, 1), elsewhere.hash);
    announcing.announce_block(at(8, 1), at_60.hash);
    tokio::time::sleep(PEER_POLL).await;
    let five_netgroups = published();
    announcing.announce_block(at(6, 1), at_45.hash);
    eventually("six netgroups carried 45", || published() == Some((at_60, at_45))).await;
    announcing.announce_block(at(6, 1), at_55.hash);
    eventually("finalized on six netgroups' word", || published() == Some((at_60, at_50))).await;

    assert_eq!(five_netgroups, Some((at_60, at_40)), "five netgroups: finalized unmoved at 40");
    assert!(held_by().is_empty(), "a still down: vouched nothing: {:?}", held_by());
    cancel.cancel();
}

/// A restart mid-outage: a unreachable from boot, the peer set at 180, our indexes' chain through
/// 150, `RESTARTED` later
///
/// - own = 36 ..= 150 by hash (`CONTEXT` + 2): the peers' headers after 36 match each → anchored
///   at 37, the peers' tip followed to 180 (finalized stays 37: nothing vouched, no quorum)
/// - one hash not ours: never anchored (asked again every `PEER_POLL`)
#[tokio::test(start_paused = true)]
async fn our_own_chain_anchors_the_peers_tip_with_no_trusted_validator_answering() {
    const RESTARTED: Duration = Duration::from_secs(30);
    let mut chain = MockChain::regtest();
    let at_150 = chain.mine_empty(150);
    let at_180 = chain.mine_empty(30);
    let path = chain.blocks(at_150);
    let own: Vec<BlockRef> = path[150 - CONTEXT - 1..].iter().map(|b| b.block_ref()).collect();
    let mut forged = own.clone();
    forged[65].hash = BlockHash([0xF0; 32]);
    let rows = [("ours", own, Some((at_180, chain.at(h(37))))), ("one hash forged", forged, None)];
    for (row, own, expected) in rows {
        let peer_set = Arc::new(MockPeerSet::new(&chain, at_180));
        let validator = Arc::new(chain.validator(at_180));
        validator.down(true);
        let (view, _balancer, cancel) =
            view_merging_polls(&[validator], &["a:8232"], Some(peer_set));
        let sync =
            view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(regtest()));
        let sync = sync.with_own_chain(own);
        let chains = sync.subscribe();
        tokio::spawn(sync.run(cancel.clone()));

        tokio::time::sleep(RESTARTED).await;

        let published = chains.borrow().clone().map(|c| (c.best(), c.finalized_tip()));
        assert_eq!(published, expected, "{row}: (best, finalized) published");
        cancel.cancel();
    }
}

/// a at 40 (finalized 40 once best ≥ 50), the peer set's best path above it
///
/// Each row = what the peers' path ends in, where the tip stops, whether the peers were blamed:
///
/// - 2,100 good headers: the ceiling (finalized + depth + `HEADER_BATCH` = 2,050) exactly
/// - block 45 from the future: 44, no blame (only the clock is behind)
/// - block 45 with nBits the rules refuse: 44, each refusal a misanswer on the peer set
#[tokio::test(start_paused = true)]
async fn peer_headers_stop_at_the_ceiling_wait_out_the_clock_and_a_broken_rule_is_a_misanswer() {
    use metrics_util::debugging::DebuggingRecorder;

    let blamed_peers = |key: &metrics_util::CompositeKey| {
        key.key().name() == "zaino.traffic.misanswers_total"
            && key.key().labels().any(|l| l.key() == "endpoint" && l.value() == "peers")
    };

    type PathEnd = fn(&mut MockChain) -> BlockRef;
    #[rustfmt::skip]
    let rows: [(&str, PathEnd, u32, u32, bool); 3] = [
        // row,              the peers' path ends in,                       tip,   finalized, blamed
        ("past the ceiling", |chain| chain.mine_empty(2_060),              2_050, 40,        false),
        ("from the future",  |chain| chain.mine(|b| b.time(4_000_000_000)), 44,   34,        false),
        ("broken nBits",     |chain| chain.mine(|b| b.bits(0x1d00_ffff)),  44,    34,        true),
    ];
    for (row, path_end, tip, finalized_tip, blamed) in rows {
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
        let (view, _balancer, cancel) =
            view_merging_polls(&[validator], &["a:8232"], Some(peer_set));
        let sync =
            view.header_sync(HeaderChain::new(depth())).with_peers(Params::regtest(regtest()));
        let verified = sync.subscribe();
        tokio::spawn(sync.run(cancel.clone()));
        let published = || verified.borrow().clone().map(|v| (v.best(), v.finalized_tip()));

        let expected = Some((best[tip as usize], best[finalized_tip as usize]));
        eventually(row, || published() == expected).await;
        tokio::time::sleep(PEER_POLL * 6).await;

        assert_eq!(published(), expected, "{row}: (best, finalized) stays, six peer polls later");
        let recorded = snapshotter.snapshot().into_vec();
        let peers_blamed = recorded.iter().any(|(key, ..)| blamed_peers(key));
        assert_eq!(peers_blamed, blamed, "{row}: a misanswer charged to the peer set");
        cancel.cancel();
    }
}
