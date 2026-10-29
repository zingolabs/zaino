//! Driver over `MockValidator` members (Zaino's JSON-RPC client to each), paused clock
//! (`usage.md`)

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zaino_source::testing::{h, raw_transaction, within, MockChain, MockValidator};
use zaino_source::{
    BlockLink, ChainDataSource, FailureMode, GetAtHeightError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, QueryError, SendRawTransactionError, ZcashLegacyRpcClient,
};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, TxId};

use crate::balancer::STALL_WARN_EVERY;
use crate::class::Class;
use crate::member::{
    DOWN_AFTER, METADATA_REFRESH, MIN_POLL_SPACING, POLL_INTERVAL, STREAMED_POLL_INTERVAL,
};
use crate::testing::MockPeerSet;
use crate::{
    HeaderAsk, Health, Limits, MemberId, Peers, Push, TrafficBalancer, TrustedValidator,
    TrustedValidatorId, Urgency,
};

fn v(index: usize) -> TrustedValidatorId {
    TrustedValidatorId::new(index).expect("small")
}

fn trusted_member(index: usize) -> MemberId {
    MemberId::TrustedValidator(v(index))
}

/// `validator` as a trusted member at `priority` (8 connections)
fn trusted(
    address: &str,
    validator: &MockValidator,
    priority: u8,
) -> TrustedValidator<ZcashLegacyRpcClient> {
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let source = Arc::new(validator.client());
    TrustedValidator { address: address.to_owned(), source, priority, limits }
}

/// Balancer over `trusted` (+ `peers`), its driver spawned (runs once the test awaits)
fn balancer_running(
    trusted: Vec<TrustedValidator<ZcashLegacyRpcClient>>,
    peers: Option<Peers>,
) -> (TrafficBalancer<ZcashLegacyRpcClient>, CancellationToken) {
    let (balancer, driver) = TrafficBalancer::new(trusted, peers);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    (balancer, cancel)
}

/// Height of each answered header on `chain`'s path to 3 (`None` = a header off it)
fn header_heights<E>(
    chain: &MockChain,
    links: &[Result<BlockLink, E>],
) -> Vec<Result<Option<BlockHeight>, E>>
where
    E: Clone,
{
    let height_of = |link: &BlockLink| {
        (0..=3).map(h).find(|height| chain.header_bytes(chain.at(*height).hash) == link.header)
    };
    links.iter().map(|link| link.as_ref().map(height_of).map_err(E::clone)).collect()
}

#[tokio::test(start_paused = true)]
async fn a_lookup_absent_on_the_best_tier_moves_on_and_absent_everywhere_ends_absent() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let (txid, raw) = raw_transaction(1, 0);
    chain.submit_raw(raw.clone());
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    partner.hear(&chain, txid);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let transaction = |txid: TxId| {
        balancer.lookup(move |source| async move { source.get_transaction(txid).await })
    };
    let unknown = TxId::from_bytes([7; 32]);

    let found = within("a lookup", transaction(txid)).await.expect("partner holds it");
    let absent = within("a lookup", transaction(unknown)).await.expect_err("nobody holds it");

    assert_eq!(found.ticket.member, trusted_member(1), "absent on ours (priority 0): partner's");
    assert_eq!(found.value.bytes, raw, "partner's bytes = the submitted transaction");
    let expected = GetTransactionError::NotFound(unknown);
    let last = absent.last;
    let said_absent = matches!(&last, Some(QueryError::Domain(said)) if *said == expected);
    assert!(said_absent, "unknown everywhere: the absence ends it: {last:?}");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_lookup_lacking_its_method_moves_on_and_lacking_everywhere_is_unsupported_not_failed() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let (sent_id, sent) = raw_transaction(2, 0);
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    ours.lacks(&["checkadmission"]);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    for validator in [v(0), v(1)] {
        within("a poll", balancer.observe(validator).changed()).await.expect("driver running");
    }
    let check = || {
        let sent = sent.clone();
        balancer.lookup(move |source| {
            let sent = sent.clone();
            async move { source.check_admission(sent).await }
        })
    };

    let checked = within("a lookup", check()).await.expect("partner has the method");
    partner.lacks(&["checkadmission"]);
    let lacked = within("a lookup", check()).await.expect_err("nobody has it");

    assert_eq!(checked.ticket.member, trusted_member(1), "ours lacks checkadmission: partner");
    assert_eq!(checked.value, sent_id, "partner admits it");
    let last = lacked.last;
    assert!(matches!(last, Some(QueryError::Unsupported)), "lacked everywhere: {last:?}");
    // one charged failure = `Degraded` = out of `entries()`
    assert_eq!(balancer.entries(), [v(0), v(1)], "a missing method charges no failure");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn pinned_headers_come_from_the_named_validator_even_below_the_best_tier() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);

    let pinned = HeaderAsk::Pinned { validator: v(1), heights: vec![h(0), h(3), h(4)] };
    let links = within("headers", balancer.headers(pinned)).await.expect("partner answers");

    assert_eq!(links.ticket.member, trusted_member(1), "pinned to partner (priority 1)");
    let answered = header_heights(&chain, &links.value);
    let expected =
        vec![Ok(Some(h(0))), Ok(Some(h(3))), Err(GetAtHeightError::HeightNotFound(h(4)))];
    assert_eq!(answered, expected, "partner's headers asked at 0, 3, 4 (past its tip)");
    assert_eq!(ours.calls("getblockheader"), 0, "ours never asked");
    cancel.cancel();
}

/// Partner preferred and holding it: ours (best tier) still asked, answering not listed
#[tokio::test(start_paused = true)]
async fn bytes_go_to_the_best_tier_before_a_preferred_lower_one() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let (txid, raw) = raw_transaction(1, 0);
    chain.submit_raw(raw.clone());
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    partner.hear(&chain, txid);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let listed = MempoolListed {
        txid,
        fee: chain.mempool()[0].fee,
        encoded_len: u32::try_from(raw.len()).expect("one small tx"),
    };

    let bytes = balancer.bytes(vec![listed], vec![trusted_member(1)]);
    let bytes = within("bytes", bytes).await.expect("answered");

    assert_eq!(
        bytes.ticket.member,
        trusted_member(0),
        "ours (priority 0) before preferred partner"
    );
    let not_listed = vec![Err(GetRawMempoolTransactionError::NotFound(txid))];
    assert_eq!(bytes.value, not_listed, "ours never heard it: its NotFound for the item");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_submit_reaches_only_its_named_member_and_a_malformed_one_ends_with_the_rejection() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let (sent_id, sent) = raw_transaction(2, 0);
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let send = |validator, raw: Vec<u8>| {
        balancer.submit(validator, move |source| {
            let raw = raw.clone();
            async move { source.send_raw_transaction(raw).await }
        })
    };

    let accepted = within("a submit", send(v(1), sent)).await.expect("well-formed");
    let refused = within("a submit", send(v(0), vec![9; 8])).await.expect_err("malformed");

    assert_eq!(accepted.ticket.member, trusted_member(1), "submitted to partner (priority 1)");
    assert_eq!(accepted.value, sent_id, "partner's txid");
    let pushes = [ours.calls("sendrawtransaction"), partner.calls("sendrawtransaction")];
    assert_eq!(pushes, [1, 1], "sendrawtransaction calls (ours, partner): one each, as named");
    let last = refused.last;
    let malformed = matches!(last, Some(QueryError::Domain(SendRawTransactionError::Malformed(_))));
    assert!(malformed, "ours' rejection of 8 garbage bytes ends it: {last:?}");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_reported_liar_is_benched_and_the_re_ask_by_hash_or_height_goes_to_the_honest_one() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let (liar, honest) = (chain.validator(tip), chain.validator(tip));
    let members = vec![trusted("liar", &liar, 0), trusted("honest", &honest, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let hash = chain.at(h(2)).hash;
    let first = within("a block", balancer.block(hash, Urgency::Tip)).await;
    assert_eq!(first.ticket.member, trusted_member(0), "the liar (priority 0) answers first");

    balancer.report(first.ticket, &std::io::Error::other("merkle root mismatch"));

    let by_hash = within("a block by hash", balancer.block(hash, Urgency::Bulk)).await;
    let by_height = balancer.blocks_at(Arc::from([h(2)]), Urgency::Bulk);
    let by_height = within("a block by height", by_height).await;
    assert_eq!(by_hash.ticket.member, trusted_member(1), "by hash: the benched liar skipped");
    assert_eq!(by_height.ticket.member, trusted_member(1), "by height: the benched liar skipped");
    let served = [by_hash.value.header().hash, by_height.value[0].header().hash];
    assert_eq!(served, [hash, hash], "block 2 both ways");
    within("honest polled", balancer.observe(v(1)).changed()).await.expect("driver running");
    assert_eq!(
        balancer.entries(),
        [v(1)],
        "both polled live, the benched liar enters no submission"
    );
    cancel.cancel();
}

/// Ours: `STALL` per ask (polls at once); partner: at once
#[tokio::test(start_paused = true)]
async fn a_tip_block_is_hedged_to_another_member_at_its_floor_not_after_the_stall() {
    const STALL: Duration = Duration::from_secs(20);
    let floor = Class::TipBlock.hedge_floor().expect("tip blocks hedge");
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let ours = chain.validator(tip);
    ours.latency(&["getblock"], STALL);
    let partner = chain.validator(tip);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let started = Instant::now();

    let hedged = within("a hedged block", balancer.block(tip.hash, Urgency::Tip)).await;

    let waited = started.elapsed();
    assert_eq!(hedged.ticket.member, trusted_member(1), "partner's answer wins the hedge");
    assert_eq!(
        waited, floor,
        "answered at the {floor:?} hedge floor, not after the {STALL:?} stall"
    );
    cancel.cancel();
}

/// 64 lookups pile onto ours (priority 0, `getrawtransaction` stalled 20 s): T10
#[tokio::test(start_paused = true)]
async fn a_lookup_storm_never_delays_a_poll() {
    const WINDOW: Duration = Duration::from_secs(10);
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let ours = chain.validator(tip);
    ours.latency(&["getrawtransaction"], Duration::from_secs(20));
    let partner = chain.validator(tip);
    let members = vec![trusted("ours", &ours, 0), trusted("partner", &partner, 1)];
    let (balancer, cancel) = balancer_running(members, None);
    let mut polls = balancer.observe(v(0));
    let storm: Vec<_> = (0..64u8)
        .map(|n| {
            let balancer = balancer.clone();
            let txid = TxId::from_bytes([n; 32]);
            tokio::spawn(async move {
                balancer
                    .lookup(move |source| async move { source.get_transaction(txid).await })
                    .await
            })
        })
        .collect();

    let deadline = Instant::now() + WINDOW;
    let mut polled = 0;
    while tokio::time::timeout_at(deadline, polls.changed()).await.is_ok() {
        polled += 1;
    }

    let waiting = storm.iter().filter(|lookup| !lookup.is_finished()).count();
    assert_eq!(waiting, 64, "lookups still waiting on ours' stalled answers");
    let cadence = u32::try_from(WINDOW.as_nanos() / POLL_INTERVAL.as_nanos()).expect("few polls");
    assert!(
        polled >= cadence - 1,
        "{polled} polls of ours in {WINDOW:?} of 64 lookups, want ≥ {} (each {POLL_INTERVAL:?})",
        cadence - 1
    );
    storm.iter().for_each(|lookup| lookup.abort());
    cancel.cancel();
}

/// One member refusing `getblock` (polls fine)
///
/// - block ask retries its rounds, a warning each `STALL_WARN_EVERY` naming member + last cause
/// - served once reachable, one line saying so
#[tokio::test(start_paused = true)]
async fn a_block_no_member_serves_retries_forever_and_warns_each_minute() {
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer poisoned").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let lines = |captured: &Captured| -> Vec<String> {
        let bytes = captured.0.lock().expect("log buffer poisoned").clone();
        String::from_utf8(bytes).expect("utf-8 logs").lines().map(str::to_owned).collect()
    };

    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let ours = chain.validator(tip);
    ours.reachable(&["getblock"], false);
    let (balancer, cancel) = balancer_running(vec![trusted("ours", &ours, 0)], None);
    let asking = {
        let balancer = balancer.clone();
        tokio::spawn(async move { balancer.block(tip.hash, Urgency::Bulk).await })
    };

    tokio::time::sleep(STALL_WARN_EVERY * 2 + Duration::from_secs(5)).await;

    let warnings: Vec<String> =
        lines(&captured).into_iter().filter(|line| line.contains("Ask unanswered")).collect();
    assert_eq!(warnings.len(), 2, "one per {STALL_WARN_EVERY:?}, 2 periods open: {warnings:#?}");
    for warning in &warnings {
        for field in ["WARN", "class=\"bulk_block\"", "member=Some(TrustedValidator(", "503"] {
            assert!(warning.contains(field), "warning lacks {field}: {warning}");
        }
    }
    assert!(!asking.is_finished(), "never unanswered: still retrying");

    ours.reachable(&["getblock"], true);
    let served = within("the block served", asking).await.expect("ask task joins");
    assert_eq!(served.ticket.member, trusted_member(0), "ours, once reachable");
    assert_eq!(served.value.header().hash, tip.hash, "the tip asked for");
    let answered: Vec<String> =
        lines(&captured).into_iter().filter(|l| l.contains("Stalled ask answered")).collect();
    assert_eq!(answered.len(), 1, "one line once served: {answered:#?}");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_streaming_member_is_polled_every_15_s_and_a_push_event_polls_within_200_ms() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    let (balancer, cancel) = balancer_running(vec![trusted("validator", &validator, 0)], None);
    let mut observed = balancer.observe(v(0));
    within("the first poll", observed.changed()).await.expect("driver running");
    balancer.pushed(v(0), Push::Link(true));
    within("the poll the link wakes", observed.changed()).await.expect("driver running");
    let streaming = observed.borrow_and_update().as_ref().map(|o| o.streaming);
    assert_eq!(streaming, Some(true), "the link's poll reports the stream up");

    let linked = Instant::now();
    within("a reconcile poll", observed.changed()).await.expect("driver running");
    let reconciled = linked.elapsed();
    let pushed = Instant::now();
    balancer.pushed(v(0), Push::Changed);
    within("the poll a push wakes", observed.changed()).await.expect("driver running");
    let woken = pushed.elapsed();

    assert_eq!(reconciled, STREAMED_POLL_INTERVAL, "streaming: the next poll at its interval");
    assert!(woken <= MIN_POLL_SPACING, "push → poll after {woken:?}, want ≤ {MIN_POLL_SPACING:?}");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_new_best_rides_the_next_poll_and_wakes_none() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    let (balancer, cancel) = balancer_running(vec![trusted("validator", &validator, 0)], None);
    let mut observed = balancer.observe(v(0));
    within("the first poll", observed.changed()).await.expect("driver running");
    let first = Instant::now();

    balancer.poll_best(|| Some(h(2)));

    within("the next poll", observed.changed()).await.expect("driver running");
    let after = first.elapsed();
    let observation = observed.borrow_and_update().clone().expect("polled");
    assert_eq!(
        after, POLL_INTERVAL,
        "the next poll at its interval (a wake = {MIN_POLL_SPACING:?})"
    );
    assert_eq!(observation.asked, Some(h(2)), "getblockhash asked at the new best");
    let held: Vec<Option<BlockHash>> = match &observation.polled {
        Ok(reading) => reading.held.iter().map(|held| held.as_ref().ok().copied()).collect(),
        Err(cause) => panic!("reachable, yet the poll failed: {cause}"),
    };
    assert_eq!(held, [Some(chain.at(h(2)).hash)], "its hash at 2");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn failing_polls_walk_degraded_to_down_after_ten_and_the_first_answer_back_is_live() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    let (balancer, cancel) = balancer_running(vec![trusted("validator", &validator, 0)], None);
    let mut observed = balancer.observe(v(0));
    within("the first poll", observed.changed()).await.expect("driver running");
    let health = observed.borrow_and_update().as_ref().map(|o| o.health);
    assert_eq!(health, Some(Health::Live), "answered: live");

    validator.down(true);
    let mut walked = Vec::new();
    for _ in 0..DOWN_AFTER {
        within("a failed poll", observed.changed()).await.expect("driver running");
        let observation = observed.borrow_and_update().clone().expect("polled");
        walked.push((observation.polled.as_ref().err().map(|e| e.mode), observation.health));
    }
    validator.down(false);
    within("the probe answered", observed.changed()).await.expect("driver running");

    let refused = Some(FailureMode::HttpStatus(503));
    let degraded = usize::try_from(DOWN_AFTER - 1).expect("u32 fits usize");
    let mut expected = vec![(refused, Health::Degraded); degraded];
    expected.push((refused, Health::Down));
    assert_eq!(walked, expected, "each failed poll: (cause, health right after it)");
    let back = observed.borrow_and_update().clone().expect("polled");
    assert!(back.polled.is_ok(), "reachable again: {:?}", back.polled.as_ref().err());
    assert_eq!(back.health, Health::Live, "the probe's answer");
    cancel.cancel();
}

/// Metadata due at `METADATA_REFRESH`: the poll then fails, the next answer still carries it
#[tokio::test(start_paused = true)]
async fn a_failed_poll_leaves_metadata_due_for_the_next_answer() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    let (balancer, cancel) = balancer_running(vec![trusted("validator", &validator, 0)], None);
    let mut observed = balancer.observe(v(0));
    within("the first poll", observed.changed()).await.expect("driver running");
    let first = Instant::now();
    type Observed = tokio::sync::watch::Receiver<Option<Arc<crate::Observation>>>;
    let metadata = |observed: &mut Observed| {
        let observation = observed.borrow_and_update().clone().expect("polled");
        let polled = observation.polled.as_ref().map(|reading| reading.metadata.is_some());
        polled.map_err(|cause| cause.mode)
    };
    assert_eq!(metadata(&mut observed), Ok(true), "the first poll reads metadata");

    tokio::time::sleep_until(first + METADATA_REFRESH - POLL_INTERVAL / 2).await;
    let before = metadata(&mut observed);
    validator.down(true);
    within("the poll metadata is due on", observed.changed()).await.expect("driver running");
    let failed = metadata(&mut observed);
    validator.down(false);
    within("the next poll", observed.changed()).await.expect("driver running");
    let after = metadata(&mut observed);

    let refused = Err(FailureMode::HttpStatus(503));
    assert_eq!([before, failed, after], [Ok(false), refused, Ok(true)], "metadata read per poll");
    cancel.cancel();
}

/// Trusted member down from the start, the peer set holding blocks 0..=2
#[tokio::test(start_paused = true)]
async fn the_peer_set_serves_tip_blocks_and_headers_and_a_report_never_benches_it() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    validator.down(true);
    let peer_set = Arc::new(MockPeerSet::new(&chain, chain.at(h(2))));
    let peers = Peers { transport: peer_set, in_flight: 4 };
    let (balancer, cancel) =
        balancer_running(vec![trusted("validator", &validator, 0)], Some(peers));

    let at_1 = chain.at(h(1)).hash;
    let served = within("block 1", balancer.block(at_1, Urgency::Tip)).await;
    balancer.report(served.ticket, &std::fmt::Error);
    let after = within("block 2 after a report", balancer.block(chain.at(h(2)).hash, Urgency::Tip));
    let after = after.await;
    let ask = HeaderAsk::Peers { locator: vec![chain.genesis().hash] };
    let headers =
        within("peer headers", balancer.headers(ask)).await.expect("the peer set answers");

    assert_eq!(served.ticket.member, MemberId::Peers, "trusted down: block 1 from the peer set");
    assert_eq!(served.value.header().hash, at_1, "block 1");
    assert_eq!(after.ticket.member, MemberId::Peers, "a report never benches the peer set");
    assert_eq!(headers.ticket.member, MemberId::Peers, "peer headers");
    let answered = header_heights(&chain, &headers.value);
    assert_eq!(answered, vec![Ok(Some(h(1))), Ok(Some(h(2)))], "after the genesis locator, to 2");
    cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_lookup_with_its_only_member_down_ends_with_that_transport_failure() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    validator.down(true);
    let (balancer, cancel) = balancer_running(vec![trusted("validator", &validator, 0)], None);
    let txid = TxId::from_bytes([1; 32]);

    let lookup = balancer.lookup(move |source| async move { source.get_transaction(txid).await });
    let lookup = within("a lookup", lookup).await.expect_err("trusted down");

    let last = lookup.last;
    let mode = match &last {
        Some(QueryError::NonDomain(cause)) => Some(cause.mode),
        _ => None,
    };
    assert_eq!(mode, Some(FailureMode::HttpStatus(503)), "the trusted failure as last: {last:?}");
    cancel.cancel();
}

/// Peer set holding block 1, the trusted member down: by height = trusted only
#[tokio::test(start_paused = true)]
async fn a_block_by_height_never_reaches_the_peer_set_and_waits_for_a_trusted_member() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    validator.down(true);
    let peers = Peers { transport: Arc::new(MockPeerSet::new(&chain, tip)), in_flight: 4 };
    let (balancer, cancel) =
        balancer_running(vec![trusted("validator", &validator, 0)], Some(peers));
    let asking = {
        let balancer = balancer.clone();
        tokio::spawn(async move { balancer.blocks_at(Arc::from([h(1)]), Urgency::Bulk).await })
    };
    tokio::time::sleep(STALL_WARN_EVERY / 2).await;
    assert!(!asking.is_finished(), "pending while its only trusted member is down");

    validator.down(false);

    let served = within("block 1 by height", asking).await.expect("ask task joins");
    assert_eq!(served.ticket.member, trusted_member(0), "the trusted member back, never the peers");
    assert_eq!(served.value[0].header().hash, chain.at(h(1)).hash, "block 1");
    cancel.cancel();
}

#[test]
#[should_panic(expected = "poll_best set twice")]
fn poll_best_set_twice_panics() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.genesis());
    let (balancer, _driver) = TrafficBalancer::new(vec![trusted("unpolled", &validator, 0)], None);
    balancer.poll_best(|| None);
    balancer.poll_best(|| None);
}
