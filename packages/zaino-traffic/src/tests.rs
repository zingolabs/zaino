//! Driver over `MockChain` members, paused clock (`traffic-balancer.md` §8)

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zaino_primitives::testing::{encode_header, Chain};
use zaino_primitives::types::{Block, BlockHash, Height, TransactionId, Zatoshis};
use zaino_source::mock::{fixture_transactions, MockChain, MEMPOOL_FEE};
use zaino_source::{
    BlockLink, ChainDataSource, FailureMode, GetAtHeightError, GetBlockByHashError,
    GetRawMempoolTransactionError, GetTransactionError, MempoolListed, NonDomainError, PollReading,
    QueryError, RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};

use crate::{
    HeaderAsk, Health, Limits, MemberId, Membership, PeerId, PeerTransport, Push, TrafficBalancer,
    Trusted, Urgency, ValidatorId,
};

/// `MockChain` answering every ask after `delay` (polls at once)
struct Delayed {
    chain: MockChain,
    delay: Duration,
}

impl ChainDataSource for Delayed {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        tokio::time::sleep(self.delay).await;
        self.chain.get_block_by_hash(hash).await
    }

    async fn get_block_links(
        &self,
        heights: &[Height],
    ) -> Result<zaino_source::BlockLinks, NonDomainError> {
        tokio::time::sleep(self.delay).await;
        self.chain.get_block_links(heights).await
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.chain.get_poll_reading(metadata, holds).await
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        tokio::time::sleep(self.delay).await;
        self.chain.get_raw_mempool_transactions(listed).await
    }

    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        tokio::time::sleep(self.delay).await;
        self.chain.get_transaction(txid).await
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        tokio::time::sleep(self.delay).await;
        self.chain.send_raw_transaction(transaction).await
    }
}

fn v(index: usize) -> ValidatorId {
    ValidatorId::new(index).expect("small")
}

fn h(height: u32) -> Height {
    Height::try_from(height).expect("small")
}

/// Ours (priority 0, lacks the transaction) + partner (priority 1, holds it):
/// - lookup: absent on ours → partner; unknown everywhere → unanswered with the absence
/// - pinned headers, bytes from the best tier (ours: not listed there = `NotFound` item)
/// - submit: pinned, accepted; garbage = unanswered with the rejection
/// - block: ours first; reported → benched (table, entries), the re-ask served by partner
#[tokio::test(start_paused = true)]
async fn each_answer_names_its_sender_and_a_reported_liar_is_benched_until_another_serves() {
    let mut chain = Chain::new();
    let tip = chain.extend(chain.genesis().hash, 3);
    let blocks = chain.path(tip.hash);
    let mut raws = fixture_transactions(2_000_000);
    let (raw, sent) = (raws.swap_remove(1), raws.swap_remove(1));
    let txid = zaino_source::prepare_transaction(&raw).expect("decodes").txid;
    let partner = MockChain::serving(blocks.clone());
    partner.mempool_insert(txid, raw.clone());
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let (balancer, driver) = TrafficBalancer::new(
        vec![
            Trusted { source: Arc::new(MockChain::serving(blocks.clone())), priority: 0, limits },
            Trusted { source: Arc::new(partner), priority: 1, limits },
        ],
        None,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let found = balancer.transaction(txid).await.expect("partner holds it");
    assert_eq!((found.from, found.value.bytes), (MemberId::Trusted(v(1)), raw.clone()));
    let unknown = TransactionId::from([7; 32]);
    let absent = balancer.transaction(unknown).await.expect_err("nobody holds it").last;
    let expected = GetTransactionError::NotFound(unknown);
    assert!(matches!(absent, Some(QueryError::Domain(said)) if said == expected));

    let pinned = HeaderAsk::Pinned { member: v(0), heights: vec![h(0), h(3), h(4)] };
    let links = balancer.headers(pinned).await.expect("ours answers");
    let link = |height: usize| Ok(BlockLink { header: encode_header(blocks[height].header()) });
    let expected = vec![link(0), link(3), Err(GetAtHeightError::HeightNotFound(h(4)))];
    assert_eq!((links.from, links.value), (MemberId::Trusted(v(0)), expected));
    let fee = Zatoshis::new(MEMPOOL_FEE).expect("in supply");
    let listed = MempoolListed { txid, fee, encoded_len: raw.len() as u32 };
    let bytes =
        balancer.bytes(vec![listed], vec![MemberId::Trusted(v(1))]).await.expect("answered");
    let not_listed = vec![Err(GetRawMempoolTransactionError::NotFound(txid))];
    assert_eq!((bytes.from, bytes.value), (MemberId::Trusted(v(0)), not_listed), "best tier first");

    let accepted = balancer.submit(v(1), sent.clone()).await.expect("well-formed");
    let sent_id = zaino_source::prepare_transaction(&sent).expect("decodes").txid;
    assert_eq!((accepted.from, accepted.value), (MemberId::Trusted(v(1)), sent_id));
    let refused = balancer.submit(v(0), vec![9; 8]).await.expect_err("malformed").last;
    assert!(matches!(refused, Some(QueryError::Domain(SendRawTransactionError::Malformed(_)))));

    let hash = blocks[2].header().hash;
    let first = balancer.block(hash, Urgency::Tip).await;
    assert_eq!((first.from, first.value.header().hash), (MemberId::Trusted(v(0)), hash));
    balancer.report(first.ticket, &std::io::Error::other("merkle root mismatch"));
    let again = balancer.block(hash, Urgency::Bulk).await;
    assert_eq!(again.from, MemberId::Trusted(v(1)), "the reported member benched");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let table = balancer.members().borrow().clone();
    let benched: Vec<bool> = table.rows.iter().map(|row| row.benched_until.is_some()).collect();
    assert_eq!((benched, balancer.entries()), (vec![true, false], vec![v(1)]));
    cancel.cancel();
}

/// Ours stalls 20 s per ask (priority 0), partner answers at once (priority 1):
/// - a tip block hedged to partner at the 2 s floor, not after the stall
/// - 64 concurrent lookups never delay a poll: ours polled ≥ 9 times in 10 s, its in flight
///   within its 8 connections (T1 + T10)
#[tokio::test(start_paused = true)]
async fn a_hedge_beats_a_stall_and_a_wallet_storm_never_delays_a_poll() {
    let mut chain = Chain::new();
    let tip = chain.extend(chain.genesis().hash, 3);
    let blocks = chain.path(tip.hash);
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let ours =
        Delayed { chain: MockChain::serving(blocks.clone()), delay: Duration::from_secs(20) };
    let partner = Delayed { chain: MockChain::serving(blocks.clone()), delay: Duration::ZERO };
    let (balancer, driver) = TrafficBalancer::new(
        vec![
            Trusted { source: Arc::new(ours), priority: 0, limits },
            Trusted { source: Arc::new(partner), priority: 1, limits },
        ],
        None,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));

    let started = Instant::now();
    let hedged = balancer.block(blocks[3].header().hash, Urgency::Tip).await;
    let waited = started.elapsed();
    assert_eq!(hedged.from, MemberId::Trusted(v(1)));
    assert!((Duration::from_secs(2)..Duration::from_millis(2_100)).contains(&waited), "{waited:?}");

    let mut polls = balancer.observe(v(0));
    let storm: Vec<_> = (0..64u8)
        .map(|n| {
            let balancer = balancer.clone();
            tokio::spawn(async move { balancer.transaction(TransactionId::from([n; 32])).await })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut polled = 0;
    while tokio::time::timeout_at(deadline, polls.changed()).await.is_ok() {
        polled += 1;
        let ours = balancer.members().borrow().rows[0].in_flight;
        assert!(ours <= 8, "T1: {ours} in flight on 8 connections");
    }
    assert!(polled >= 9, "T10: {polled} polls in 10 s under a lookup storm");
    storm.iter().for_each(|lookup| lookup.abort());
    cancel.cancel();
}

/// One member, its push stream up:
/// - reconcile every 15 s, a push event polls within 200 ms
/// - poll heights ride the next poll (`getblockhash` per height)
/// - unreachable: `Degraded`, then `Down` after 10 failures (ladder); reachable: `Live` again
#[tokio::test(start_paused = true)]
async fn pushes_wake_the_poll_heights_ride_it_and_failures_walk_the_health_ladder() {
    let mut chain = Chain::new();
    let tip = chain.extend(chain.genesis().hash, 3);
    let blocks = chain.path(tip.hash);
    let mock = Arc::new(MockChain::serving(blocks.clone()));
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&mock), priority: 0, limits }];
    let (balancer, driver) = TrafficBalancer::new(trusted, None);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    let mut observed = balancer.observe(v(0));
    observed.changed().await.expect("driver running");

    balancer.pushed(v(0), Push::Link(true));
    observed.changed().await.expect("driver running");
    let streaming = observed.borrow_and_update().as_ref().map(|o| o.streaming);
    assert_eq!(streaming, Some(true));
    let quiet = tokio::time::timeout(Duration::from_secs(10), observed.changed()).await;
    assert!(quiet.is_err(), "streaming: no poll for 10 s");
    let pushed = Instant::now();
    balancer.pushed(v(0), Push::Changed);
    observed.changed().await.expect("driver running");
    assert!(pushed.elapsed() <= Duration::from_millis(200), "{:?}", pushed.elapsed());

    balancer.ask_each_poll(vec![h(2), h(9)]);
    observed.changed().await.expect("driver running");
    let observation = observed.borrow_and_update().clone().expect("polled");
    let held: Vec<Option<BlockHash>> = match &observation.polled {
        Ok(reading) => reading.held.iter().map(|held| held.as_ref().ok().copied()).collect(),
        Err(cause) => panic!("reachable: {cause}"),
    };
    assert_eq!(
        (observation.asked.clone(), held),
        (vec![h(2), h(9)], vec![Some(blocks[2].header().hash), None])
    );

    let health = |balancer: &TrafficBalancer<MockChain>| balancer.members().borrow().rows[0].health;
    assert_eq!(health(&balancer), Health::Live);
    mock.set_reachable(false);
    tokio::time::sleep(Duration::from_secs(16)).await;
    assert_eq!(health(&balancer), Health::Degraded);
    type Observed = tokio::sync::watch::Receiver<Option<Arc<crate::Observation>>>;
    let observed_health = |observed: &mut Observed| {
        let observation = observed.borrow_and_update().clone();
        observation.map(|o| (o.polled.as_ref().err().map(|e| e.mode.clone()), o.health))
    };
    let failed = Some((Some(FailureMode::Connection), Health::Degraded));
    assert_eq!(observed_health(&mut observed), failed, "the health it left the member in");
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert_eq!(health(&balancer), Health::Down);
    assert_eq!(observed_health(&mut observed), Some((Some(FailureMode::Connection), Health::Down)));
    mock.set_reachable(true);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(health(&balancer), Health::Live, "the probe brings it back");
    assert_eq!(observed_health(&mut observed), Some((None, Health::Live)));
    cancel.cancel();
}

/// Peers from the WorkPool: block bodies + headers (checkable), never lookups
struct Pool {
    blocks: Vec<Block>,
    membership: Mutex<Option<BoxStream<'static, Membership>>>,
}

impl PeerTransport for Pool {
    fn headers(
        &self,
        _: PeerId,
        _: Vec<BlockHash>,
        _: Option<BlockHash>,
    ) -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>> {
        let headers = self.blocks.iter().map(|block| encode_header(block.header())).collect();
        futures::future::ready(Ok(headers)).boxed()
    }

    fn block(
        &self,
        _: PeerId,
        hash: BlockHash,
    ) -> BoxFuture<'static, Result<Block, NonDomainError>> {
        let block = self.blocks.iter().find(|block| block.header().hash == hash).cloned();
        let block = block.ok_or(NonDomainError::new(FailureMode::RpcError(0), "notfound"));
        futures::future::ready(block).boxed()
    }

    fn transactions(
        &self,
        _: PeerId,
        ids: Vec<TransactionId>,
    ) -> BoxFuture<'static, Result<Vec<Option<Vec<u8>>>, NonDomainError>> {
        futures::future::ready(Ok(vec![None; ids.len()])).boxed()
    }

    fn joined_left(&self) -> BoxStream<'static, Membership> {
        self.membership.lock().expect("pool mutex").take().expect("subscribed once")
    }
}

/// Trusted member down from the start, one peer joined:
/// - tip block + peer headers served by the peer (`Peer(3)`)
/// - lookup never reaches it: unanswered, the trusted member's transport failure as `last`
/// - the peer leaves: a block ask pends (no member left), served once the trusted member is back
#[tokio::test(start_paused = true)]
async fn peers_serve_checkable_asks_and_never_lookups() {
    let mut chain = Chain::new();
    let tip = chain.extend(chain.genesis().hash, 3);
    let blocks = chain.path(tip.hash);
    let mock = Arc::new(MockChain::serving(blocks.clone()));
    mock.set_reachable(false);
    let (joins, membership) = futures::channel::mpsc::unbounded();
    let pool = Pool { blocks: blocks.clone(), membership: Mutex::new(Some(membership.boxed())) };
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&mock), priority: 0, limits }];
    let (balancer, driver) = TrafficBalancer::new(trusted, Some(Arc::new(pool)));
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    joins.unbounded_send(Membership::Joined(PeerId(3))).expect("driver running");

    let peer = MemberId::Peer(PeerId(3));
    let hash = blocks[1].header().hash;
    let served = balancer.block(hash, Urgency::Tip).await;
    assert_eq!((served.from, served.value.header().hash), (peer, hash));
    let ask = HeaderAsk::Peers { locator: vec![blocks[0].header().hash], stop: None };
    let headers = balancer.headers(ask).await.expect("the peer answers");
    assert_eq!((headers.from, headers.value.len()), (peer, blocks.len()));
    let lookup =
        balancer.transaction(TransactionId::from([1; 32])).await.expect_err("trusted down");
    let mode = match lookup.last {
        Some(QueryError::NonDomain(cause)) => Some(cause.mode),
        _ => None,
    };
    assert_eq!(mode, Some(FailureMode::Connection), "never asked of the peer");

    joins.unbounded_send(Membership::Left(PeerId(3))).expect("driver running");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let pending = balancer.block(hash, Urgency::Bulk);
    tokio::pin!(pending);
    let none = tokio::time::timeout(Duration::from_secs(10), &mut pending).await;
    assert!(none.is_err(), "no member to serve it: pending, never unanswered");
    mock.set_reachable(true);
    let back = pending.await;
    assert_eq!(back.from, MemberId::Trusted(v(0)));
    cancel.cancel();
}
