//! Driver over `MockValidator` members (Zaino's JSON-RPC client to each), paused clock
//! (`usage.md`)

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zaino_source::testing::{h, raw_transaction, within, MockChain, ValidatorViews};
use zaino_source::{
    BlockLink, ChainDataSource, FailureMode, GetAtHeightError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, QueryError, SendRawTransactionError,
};
use zcash_protocol::{BlockHash, TxId};

use crate::testing::MockPeerSet;
use crate::{
    HeaderAsk, Health, Limits, MemberId, Peers, Push, TrafficBalancer, TrustedValidator,
    TrustedValidatorId, Urgency,
};

fn v(index: usize) -> TrustedValidatorId {
    TrustedValidatorId::new(index).expect("small")
}

/// Ours (priority 0, lacks the transaction) + partner (priority 1, holds it):
/// - lookup: absent on ours → partner
/// - lookup: unknown everywhere → unanswered with the absence
/// - pinned headers, bytes from the best tier (ours: not listed there = `NotFound` item)
/// - submit: pinned, accepted
/// - submit: garbage = unanswered with the rejection
#[tokio::test(start_paused = true)]
async fn each_class_reaches_the_member_its_route_names() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let blocks = chain.blocks(tip);
    let (txid, raw) = raw_transaction(1, 0);
    let (sent_id, sent) = raw_transaction(2, 0);
    chain.submit_raw(raw.clone());
    let ours = chain.validator(tip);
    let partner = chain.validator(tip);
    partner.hear(&chain, txid);
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (balancer, driver) = TrafficBalancer::new(
        vec![
            TrustedValidator {
                address: "ours".to_owned(),
                source: Arc::new(ours.client()),
                priority: 0,
                limits,
            },
            TrustedValidator {
                address: "partner".to_owned(),
                source: Arc::new(partner.client()),
                priority: 1,
                limits,
            },
        ],
        None,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    let transaction = |txid: TxId| {
        balancer.lookup(move |source| async move { source.get_transaction(txid).await })
    };
    let standard_fee = || balancer.lookup(|source| async move { source.get_standard_fee().await });

    let found = within("a lookup", transaction(txid)).await.expect("partner holds it");
    assert_eq!(
        (found.ticket.member, found.value.bytes),
        (MemberId::TrustedValidator(v(1)), raw.clone())
    );
    let unknown = TxId::from_bytes([7; 32]);
    let absent = within("a lookup", transaction(unknown)).await.expect_err("nobody holds it").last;
    let expected = GetTransactionError::NotFound(unknown);
    assert!(matches!(absent, Some(QueryError::Domain(said)) if said == expected));

    for validator in [v(0), v(1)] {
        within("a poll", balancer.observe(validator).changed()).await.expect("driver running");
    }
    assert_eq!(balancer.entries(), [v(0), v(1)], "both polled: live");
    ours.lacks(&["getstandardfee"]);
    let fee = within("a lookup", standard_fee()).await.expect("partner has the method");
    let next = "a missing method = the next validator";
    assert_eq!(fee.ticket.member, MemberId::TrustedValidator(v(1)), "{next}");
    partner.lacks(&["getstandardfee"]);
    let lacked = within("a lookup", standard_fee()).await.expect_err("nobody has it").last;
    assert!(matches!(lacked, Some(QueryError::Unsupported)), "{lacked:?}");
    // one charged failure = `Degraded` = out of `entries()`
    assert_eq!(balancer.entries(), [v(0), v(1)], "an older node is not a failing one");

    let pinned = HeaderAsk::Pinned { validator: v(0), heights: vec![h(0), h(3), h(4)] };
    let links = within("headers", balancer.headers(pinned)).await.expect("ours answers");
    let link = |at: usize| Ok(BlockLink { header: chain.header_bytes(blocks[at].header().hash) });
    let expected = vec![link(0), link(3), Err(GetAtHeightError::HeightNotFound(h(4)))];
    assert_eq!((links.ticket.member, links.value), (MemberId::TrustedValidator(v(0)), expected));
    let listed = MempoolListed { txid, fee: chain.mempool()[0].fee, encoded_len: raw.len() as u32 };
    let bytes =
        within("bytes", balancer.bytes(vec![listed], vec![MemberId::TrustedValidator(v(1))]))
            .await
            .expect("answered");
    let not_listed = vec![Err(GetRawMempoolTransactionError::NotFound(txid))];
    let best_tier = (bytes.ticket.member, bytes.value);
    assert_eq!(best_tier, (MemberId::TrustedValidator(v(0)), not_listed), "best tier first");

    let send = |validator, raw: Vec<u8>| {
        balancer.submit(validator, move |source| {
            let raw = raw.clone();
            async move { source.send_raw_transaction(raw).await }
        })
    };
    let accepted = within("a submit", send(v(1), sent)).await.expect("well-formed");
    assert_eq!(
        (accepted.ticket.member, accepted.value),
        (MemberId::TrustedValidator(v(1)), sent_id)
    );
    let refused = within("a submit", send(v(0), vec![9; 8])).await.expect_err("malformed").last;
    assert!(matches!(refused, Some(QueryError::Domain(SendRawTransactionError::Malformed(_)))));
    cancel.cancel();
}

/// Two members at one tip, ours answers a block first
///
/// - reported → benched (table, entries)
/// - re-ask served by partner, by hash and by height alike
#[tokio::test(start_paused = true)]
async fn a_reported_liar_is_benched_until_another_serves() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (balancer, driver) = TrafficBalancer::new(
        vec![
            TrustedValidator {
                address: "liar".to_owned(),
                source: Arc::new(chain.validator(tip).client()),
                priority: 0,
                limits,
            },
            TrustedValidator {
                address: "honest".to_owned(),
                source: Arc::new(chain.validator(tip).client()),
                priority: 1,
                limits,
            },
        ],
        None,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let hash = chain.at(h(2)).hash;
    let first = within("a block", balancer.block(hash, Urgency::Tip)).await;
    assert_eq!(
        (first.ticket.member, first.value.header().hash),
        (MemberId::TrustedValidator(v(0)), hash)
    );
    balancer.report(first.ticket, &std::io::Error::other("merkle root mismatch"));
    let again = within("a block", balancer.block(hash, Urgency::Bulk)).await;
    assert_eq!(
        again.ticket.member,
        MemberId::TrustedValidator(v(1)),
        "the reported member benched"
    );
    let by_height =
        within("blocks by height", balancer.blocks_at(Arc::from([h(2)]), Urgency::Bulk)).await;
    let served = (by_height.ticket.member, by_height.value[0].header().hash);
    assert_eq!(
        served,
        (MemberId::TrustedValidator(v(1)), hash),
        "by height: the benched member skipped"
    );
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(balancer.entries(), [v(1)], "the benched member enters no submission");
    cancel.cancel();
}

/// Ours stalls 20 s per ask, polls at once (priority 0), partner answers at once (priority 1):
/// - a tip block hedged to partner at the 2 s floor, not after the stall
/// - 64 concurrent lookups never delay a poll: ours polled ≥ 9 times in 10 s (T10)
#[tokio::test(start_paused = true)]
async fn a_hedge_beats_a_stall_and_a_wallet_storm_never_delays_a_poll() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let ours = chain.validator(tip);
    let asks = ["getblockheader", "getblock", "getrawtransaction", "sendrawtransaction"];
    ours.latency(&asks, Duration::from_secs(20));
    let partner = chain.validator(tip);
    let (balancer, driver) = TrafficBalancer::new(
        vec![
            TrustedValidator {
                address: "ours".to_owned(),
                source: Arc::new(ours.client()),
                priority: 0,
                limits,
            },
            TrustedValidator {
                address: "partner".to_owned(),
                source: Arc::new(partner.client()),
                priority: 1,
                limits,
            },
        ],
        None,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let started = Instant::now();
    let hedged = within("a hedged block", balancer.block(tip.hash, Urgency::Tip)).await;
    let waited = started.elapsed();
    assert_eq!(hedged.ticket.member, MemberId::TrustedValidator(v(1)));
    assert!((Duration::from_secs(2)..Duration::from_millis(2_100)).contains(&waited), "{waited:?}");

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
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut polled = 0;
    while tokio::time::timeout_at(deadline, polls.changed()).await.is_ok() {
        polled += 1;
    }
    assert!(polled >= 9, "T10: {polled} polls in 10 s under a lookup storm");
    storm.iter().for_each(|lookup| lookup.abort());
    cancel.cancel();
}

/// One member refusing `getblock` (polls fine)
///
/// - block ask retries its rounds, a warning each minute naming the member + last cause
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
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (address, source) = ("ours".to_owned(), Arc::new(ours.client()));
    let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
    let (balancer, driver) = TrafficBalancer::new(trusted, None);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let asking = {
        let balancer = balancer.clone();
        tokio::spawn(async move { balancer.block(tip.hash, Urgency::Bulk).await })
    };
    tokio::time::sleep(Duration::from_secs(125)).await;
    let warnings: Vec<String> =
        lines(&captured).into_iter().filter(|line| line.contains("Ask unanswered")).collect();
    assert_eq!(warnings.len(), 2, "one per minute, 125 s open: {warnings:#?}");
    let named = |line: &String| {
        line.contains("WARN")
            && line.contains("class=\"bulk_block\"")
            && line.contains("member=Some(TrustedValidator(")
            && line.contains("http status 503")
    };
    assert!(warnings.iter().all(named), "level, class, member, cause: {warnings:#?}");
    assert!(!asking.is_finished(), "never unanswered: still retrying");

    ours.reachable(&["getblock"], true);
    let served = within("the block served", asking).await.expect("ask task joins");
    let served = (served.ticket.member, served.value.header().hash);
    assert_eq!(served, (MemberId::TrustedValidator(v(0)), tip.hash));
    let answered = lines(&captured).into_iter().filter(|l| l.contains("Stalled ask answered"));
    assert_eq!(answered.count(), 1, "one line once served");
    cancel.cancel();
}

/// One member, its push stream up:
/// - reconcile every 15 s, a push event polls within 200 ms
/// - `poll_best` wakes nothing, the next poll asks `getblockhash` there
/// - unreachable: `Degraded`, then `Down` after 10 failures (ladder)
/// - reachable: `Live` again, its first answer carrying the metadata the failures left due
/// - each observation: the health right after it
#[tokio::test(start_paused = true)]
async fn pushes_wake_the_poll_its_best_rides_it_and_failures_walk_the_health_ladder() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (address, source) = ("validator".to_owned(), Arc::new(validator.client()));
    let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
    let (balancer, driver) = TrafficBalancer::new(trusted, None);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    let mut observed = balancer.observe(v(0));
    within("a poll", observed.changed()).await.expect("driver running");

    balancer.pushed(v(0), Push::Link(true));
    within("a poll", observed.changed()).await.expect("driver running");
    let streaming = observed.borrow_and_update().as_ref().map(|o| o.streaming);
    assert_eq!(streaming, Some(true));
    let quiet = tokio::time::timeout(Duration::from_secs(10), observed.changed()).await;
    assert!(quiet.is_err(), "streaming: no poll for 10 s");
    let pushed = Instant::now();
    balancer.pushed(v(0), Push::Changed);
    within("a poll", observed.changed()).await.expect("driver running");
    assert!(pushed.elapsed() <= Duration::from_millis(200), "{:?}", pushed.elapsed());

    balancer.poll_best(|| Some(h(2)));
    let unwoken = tokio::time::timeout(Duration::from_secs(10), observed.changed()).await;
    assert!(unwoken.is_err(), "a new best wakes no poll");
    within("a poll", observed.changed()).await.expect("driver running");
    let observation = observed.borrow_and_update().clone().expect("polled");
    let held: Vec<Option<BlockHash>> = match &observation.polled {
        Ok(reading) => reading.held.iter().map(|held| held.as_ref().ok().copied()).collect(),
        Err(cause) => panic!("reachable: {cause}"),
    };
    assert_eq!((observation.asked, held), (Some(h(2)), vec![Some(chain.at(h(2)).hash)]));

    assert_eq!(observation.health, Health::Live);
    validator.down(true);
    tokio::time::sleep(Duration::from_secs(16)).await;
    type Observed = tokio::sync::watch::Receiver<Option<Arc<crate::Observation>>>;
    let observed_health = |observed: &mut Observed| {
        let observation = observed.borrow_and_update().clone();
        observation.map(|o| (o.polled.as_ref().err().map(|e| e.mode), o.health))
    };
    let refused = Some(FailureMode::HttpStatus(503));
    assert_eq!(
        observed_health(&mut observed),
        Some((refused, Health::Degraded)),
        "the health it left the member in"
    );
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert_eq!(observed_health(&mut observed), Some((refused, Health::Down)));
    validator.down(false);
    within("a poll", observed.changed()).await.expect("driver running");
    let back = observed.borrow_and_update().clone().expect("polled");
    let metadata = back.polled.as_ref().ok().map(|reading| reading.metadata.is_some());
    assert_eq!(metadata, Some(true), "failed polls leave metadata due: the answer back reads it");
    assert_eq!(back.health, Health::Live, "the probe");
    cancel.cancel();
}

/// Trusted member down from the start, the peer set holding blocks 0..=2 (not 3):
/// - tip block + peer headers (1, 2: after the genesis locator) served by the peer set
/// - a report on its answer benches nothing: the next block is still its
/// - lookup never reaches it: unanswered, the trusted member's transport failure as `last`
/// - a block by height never reaches it: pending until the trusted member is back
/// - a block it lacks: pending (rounds retried, never unanswered), served once the trusted is back
#[tokio::test(start_paused = true)]
async fn peers_serve_checkable_asks_and_never_lookups() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(3);
    let validator = chain.validator(tip);
    validator.down(true);
    let peers =
        Peers { transport: Arc::new(MockPeerSet::new(&chain, chain.at(h(2)))), in_flight: 4 };
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (address, source) = ("validator".to_owned(), Arc::new(validator.client()));
    let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
    let (balancer, driver) = TrafficBalancer::new(trusted, Some(peers));
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let hash = chain.at(h(1)).hash;
    let served = within("a block", balancer.block(hash, Urgency::Tip)).await;
    assert_eq!((served.ticket.member, served.value.header().hash), (MemberId::Peers, hash));
    balancer.report(served.ticket, &std::fmt::Error);
    let after = within("a block", balancer.block(chain.at(h(2)).hash, Urgency::Tip)).await;
    assert_eq!(after.ticket.member, MemberId::Peers, "a report never benches the peer set");
    let ask = HeaderAsk::Peers { locator: vec![chain.genesis().hash] };
    let headers =
        within("peer headers", balancer.headers(ask)).await.expect("the peer set answers");
    let header =
        |height: u32| Ok(BlockLink { header: chain.header_bytes(chain.at(h(height)).hash) });
    let held = vec![header(1), header(2)];
    assert_eq!((headers.ticket.member, headers.value), (MemberId::Peers, held));
    let txid = TxId::from_bytes([1; 32]);
    let lookup = balancer.lookup(move |source| async move { source.get_transaction(txid).await });
    let lookup = within("a lookup", lookup).await.expect_err("trusted down");
    let mode = match lookup.last {
        Some(QueryError::NonDomain(cause)) => Some(cause.mode),
        _ => None,
    };
    assert_eq!(mode, Some(FailureMode::HttpStatus(503)), "never asked of the peer set");
    let by_height = balancer.blocks_at(Arc::from([h(1)]), Urgency::Bulk);
    tokio::pin!(by_height);
    let unserved = tokio::time::timeout(Duration::from_secs(10), &mut by_height).await;
    assert!(unserved.is_err(), "by height: never asked of the peer set, pending for a trusted one");

    let lacking = chain.at(h(3)).hash;
    let pending = balancer.block(lacking, Urgency::Bulk);
    tokio::pin!(pending);
    let none = tokio::time::timeout(Duration::from_secs(10), &mut pending).await;
    assert!(none.is_err(), "nobody holds it: pending, never unanswered");
    validator.down(false);
    let back = within("the block once reachable", pending).await;
    let back = (back.ticket.member, back.value.header().hash);
    assert_eq!(back, (MemberId::TrustedValidator(v(0)), lacking));
    let at_one = within("by height once reachable", by_height).await;
    let at_one = (at_one.ticket.member, at_one.value[0].header().hash);
    assert_eq!(at_one, (MemberId::TrustedValidator(v(0)), hash));
    cancel.cancel();
}

#[test]
#[should_panic(expected = "poll_best set twice")]
fn poll_best_set_twice_panics() {
    let chain = MockChain::regtest();
    let source = Arc::new(chain.validator(chain.genesis()).client());
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    let trusted = TrustedValidator { address: "unpolled".to_owned(), source, priority: 0, limits };
    let (balancer, _driver) = TrafficBalancer::new(vec![trusted], None);
    balancer.poll_best(|| None);
    balancer.poll_best(|| None);
}
