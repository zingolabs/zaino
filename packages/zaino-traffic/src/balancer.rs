//! [`TrafficBalancer`]: the async face of the core (`traffic-balancer.md` §5)
//!
//! - One ask = one future: registered, runs the sends the core names, feeds each reply back
//! - Dropping it abandons the ask (its sends dropped with it, permits back)
//! - [`TrafficDriver`]: the core's clock (hedges, retries, poll cadence), polls, peers joining

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::future::{abortable, AbortHandle, BoxFuture};
use futures::stream::{self, BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt, TryFutureExt};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zaino_primitives::types::{Block, BlockHash, Height, TransactionId};
use zaino_source::{
    BlockLink, BlockLinks, ChainDataSource, GetAtHeightError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, NonDomainError, PollReading, QueryError,
    RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};

use crate::class::Class;
use crate::core::{AskId, Input, Output, PollOrder, Push, Reply, Route, Ticket, TrafficCore};
use crate::member::{Health, Limits, MemberId, MemberTable, PeerId, Synced, ValidatorId};

/// One `[[trusted_validators]]` entry (`priority`: 0 before 1 before …)
pub struct Trusted<S> {
    pub source: Arc<S>,
    pub priority: u8,
    pub limits: Limits,
}

/// `ticket` = what [`report`](TrafficBalancer::report) takes when `value` fails its check
#[derive(Debug)]
pub struct Answered<T> {
    pub value: T,
    pub from: MemberId,
    pub ticket: Ticket,
}

/// First transport failure, else the last domain answer; `None` = no eligible member to ask
#[derive(Debug)]
pub struct Unanswered<E: fmt::Debug + fmt::Display> {
    pub last: Option<QueryError<E>>,
}

impl<E: fmt::Debug + fmt::Display> fmt::Display for Unanswered<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.last {
            Some(last) => write!(f, "{last}"),
            None => f.write_str("no member to ask (benched, down or catching up)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Tip,
    Bulk,
}

/// Trusted: `getblockheader` per height, one member; peers: `FindHeaders`, any peer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderAsk {
    Pinned { member: ValidatorId, heights: Vec<Height> },
    Peers { locator: Vec<BlockHash>, stop: Option<BlockHash> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Membership {
    Joined(PeerId),
    Left(PeerId),
}

/// Peers as members (zainod implements it over `zaino-peers`)
///
/// - no answer and "not found" alike = `Err` (a peer's absence proves nothing)
pub trait PeerTransport: Send + Sync + 'static {
    fn headers(
        &self,
        peer: PeerId,
        locator: Vec<BlockHash>,
        stop: Option<BlockHash>,
    ) -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>>;
    fn block(
        &self,
        peer: PeerId,
        hash: BlockHash,
    ) -> BoxFuture<'static, Result<Block, NonDomainError>>;
    fn transactions(
        &self,
        peer: PeerId,
        ids: Vec<TransactionId>,
    ) -> BoxFuture<'static, Result<Vec<Option<Vec<u8>>>, NonDomainError>>;
    fn joined_left(&self) -> BoxStream<'static, Membership>;
}

/// One trusted member's latest poll (raw reading, latest only); `health` = right after it
///
/// - `asked` = where its `getblockhash` was asked ([`TrafficBalancer::poll_best`], read at start)
#[derive(Debug)]
pub struct Observation {
    pub member: ValidatorId,
    pub at: Instant,
    pub asked: Option<Height>,
    pub polled: Result<PollReading, NonDomainError>,
    pub streaming: bool,
    pub health: Health,
}

pub struct TrafficBalancer<S> {
    shared: Arc<Shared<S>>,
}

impl<S> Clone for TrafficBalancer<S> {
    fn clone(&self) -> Self {
        Self { shared: Arc::clone(&self.shared) }
    }
}

/// One task: run until cancelled (asks hang without it)
pub struct TrafficDriver<S> {
    shared: Arc<Shared<S>>,
}

/// The caller's best height, read as each poll starts
type PollBest = Box<dyn Fn() -> Option<Height> + Send + Sync>;

/// One finished poll: its order, the height asked, the reading
type Polled = (PollOrder, Option<Height>, Result<PollReading, NonDomainError>);

struct Shared<S> {
    state: Mutex<State>,
    trusted: Vec<Arc<S>>,
    peers: Option<Arc<dyn PeerTransport>>,
    best: Mutex<PollBest>,
    changed: Notify,
    observations: Vec<watch::Sender<Option<Arc<Observation>>>>,
    table: watch::Sender<Arc<MemberTable>>,
}

struct State {
    core: TrafficCore,
    mailboxes: HashMap<AskId, mpsc::UnboundedSender<Event>>,
    polls: Vec<PollOrder>,
    next_ask: u64,
}

/// Core output for one ask's future
enum Event {
    Send(Ticket),
    Cancel(Ticket),
    Answered(Ticket),
    Unanswered,
}

impl<S: ChainDataSource> TrafficBalancer<S> {
    /// `trusted[i]` = `ValidatorId(i)` (non-empty, ≤ `ValidatorId::MAX`)
    pub fn new(
        trusted: Vec<Trusted<S>>,
        peers: Option<Arc<dyn PeerTransport>>,
    ) -> (Self, TrafficDriver<S>) {
        let config: Vec<(u8, Limits)> = trusted.iter().map(|t| (t.priority, t.limits)).collect();
        let core = TrafficCore::new(&config, Instant::now().into_std());
        let table = watch::Sender::new(Arc::new(core.table()));
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                core,
                mailboxes: HashMap::new(),
                polls: Vec::new(),
                next_ask: 0,
            }),
            observations: trusted.iter().map(|_| watch::Sender::new(None)).collect(),
            trusted: trusted.into_iter().map(|t| t.source).collect(),
            peers,
            best: Mutex::new(Box::new(|| None)),
            changed: Notify::new(),
            table,
        });
        (Self { shared: Arc::clone(&shared) }, TrafficDriver { shared })
    }

    /// Pending until a member serves it (rounds retried); drop = abandon
    pub async fn block(&self, hash: BlockHash, urgency: Urgency) -> Answered<Block> {
        let shared = Arc::clone(&self.shared);
        let send = move |member| {
            shared.send(
                member,
                move |source| async move { source.get_block_by_hash(hash).await },
                move |peers, peer| peers.block(peer, hash),
            )
        };
        self.block_by(urgency, Route::Any, send).await
    }

    /// Best-chain block at `height` from a trusted member (peers answer by hash only); pending
    /// until one serves it; drop = abandon
    pub async fn block_at(&self, height: Height, urgency: Urgency) -> Answered<Block> {
        let shared = Arc::clone(&self.shared);
        let send = move |member| {
            let source = shared.trusted_only(member);
            async move { source.get_block_by_height(height).await }.boxed()
        };
        self.block_by(urgency, Route::Trusted, send).await
    }

    /// One block ask in `urgency`'s class (rounds retried, never unanswered)
    async fn block_by<E>(
        &self,
        urgency: Urgency,
        route: Route,
        send: impl Fn(MemberId) -> BoxFuture<'static, Result<Block, QueryError<E>>>,
    ) -> Answered<Block>
    where
        E: fmt::Debug + fmt::Display,
    {
        let class = match urgency {
            Urgency::Tip => Class::TipBlock,
            Urgency::Bulk => Class::BulkBlock,
        };
        let answered = self.ask(class, route, send).await;
        answered
            .unwrap_or_else(|_| unreachable!("block classes retry rounds, never end unanswered"))
    }

    pub async fn headers(
        &self,
        ask: HeaderAsk,
    ) -> Result<Answered<BlockLinks>, Unanswered<GetAtHeightError>> {
        let shared = Arc::clone(&self.shared);
        match ask {
            HeaderAsk::Pinned { member, heights } => {
                let send = move |to| {
                    let (source, heights) = (shared.trusted_only(to), heights.clone());
                    let links = async move { source.get_block_links(&heights).await };
                    links.map_err(QueryError::NonDomain).boxed()
                };
                self.ask(Class::Headers, Route::Only(MemberId::Trusted(member)), send).await
            }
            HeaderAsk::Peers { locator, stop } => {
                let send = move |to| {
                    let MemberId::Peer(peer) = to else {
                        unreachable!("Route::Peers reaches peers only");
                    };
                    let links = |headers: Vec<Vec<u8>>| {
                        headers.into_iter().map(|header| Ok(BlockLink { header })).collect()
                    };
                    let headers = shared.peers().headers(peer, locator.clone(), stop);
                    headers.map_ok(links).map_err(QueryError::NonDomain).boxed()
                };
                self.ask(Class::Headers, Route::Peers, send).await
            }
        }
    }

    /// One batch from one member (`prefer` first within its tier: the listers)
    pub async fn bytes(
        &self,
        listed: Vec<MempoolListed>,
        prefer: Vec<MemberId>,
    ) -> Result<Answered<RawMempoolTransactions>, Unanswered<GetRawMempoolTransactionError>> {
        let shared = Arc::clone(&self.shared);
        let send = move |member| {
            let listed = listed.clone();
            let ids: Vec<TransactionId> = listed.iter().map(|entry| entry.txid).collect();
            let found = move |raws: Vec<Option<Vec<u8>>>| {
                let raws = ids.iter().zip(raws);
                raws.map(|(id, raw)| raw.ok_or(GetRawMempoolTransactionError::NotFound(*id)))
                    .collect()
            };
            let asked = listed.iter().map(|entry| entry.txid).collect();
            shared.send(
                member,
                move |source| async move {
                    let raws = source.get_raw_mempool_transactions(&listed).await;
                    raws.map_err(QueryError::NonDomain)
                },
                move |peers, peer| peers.transactions(peer, asked).map_ok(found).boxed(),
            )
        };
        self.ask(Class::Bytes, Route::Prefer(prefer), send).await
    }

    /// Mined or in a mempool: absent → next trusted member
    pub async fn transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Answered<TransactionResponse>, Unanswered<GetTransactionError>> {
        let shared = Arc::clone(&self.shared);
        let send = move |member| {
            let source = shared.trusted_only(member);
            async move { source.get_transaction(txid).await }.boxed()
        };
        self.ask(Class::Lookup, Route::Any, send).await
    }

    /// `sendrawtransaction` on `member` alone (the caller's privacy choice); one attempt
    pub async fn submit(
        &self,
        member: ValidatorId,
        raw: Vec<u8>,
    ) -> Result<Answered<TransactionId>, Unanswered<SendRawTransactionError>> {
        let shared = Arc::clone(&self.shared);
        let send = move |to| {
            let (source, raw) = (shared.trusted_only(to), raw.clone());
            async move { source.send_raw_transaction(raw).await }.boxed()
        };
        self.ask(Class::Submit, Route::Only(MemberId::Trusted(member)), send).await
    }

    /// Trusted members a submission may enter by: live, not benched
    pub fn entries(&self) -> Vec<ValidatorId> {
        self.shared.lock().core.entries()
    }

    /// Where each poll asks `getblockhash`: `best()` as the poll starts (`None` = not asked)
    ///
    /// - never wakes a poll (an answer at a moved best = one stale fact, re-asked next poll)
    pub fn poll_best(&self, best: impl Fn() -> Option<Height> + Send + Sync + 'static) {
        *self.shared.best.lock().expect("poll best mutex poisoned") = Box::new(best);
    }

    /// A push stream's event (`IndexerWatch` callbacks): wakes `member`'s poll
    pub fn pushed(&self, member: ValidatorId, push: Push) {
        self.shared.step(Input::Push { member, push });
    }

    pub fn observe(&self, member: ValidatorId) -> watch::Receiver<Option<Arc<Observation>>> {
        let observation = self.shared.observations.get(member.get());
        observation.expect("a configured validator").subscribe()
    }

    pub fn members(&self) -> watch::Receiver<Arc<MemberTable>> {
        self.shared.table.subscribe()
    }

    /// `ticket`'s value failed the caller's check: its sender benched, and only it
    pub fn report(&self, ticket: Ticket, why: &(dyn std::error::Error + 'static)) {
        let member = match ticket.member {
            MemberId::Trusted(validator) => validator.get().to_string(),
            MemberId::Peer(_) => "peer".to_owned(),
        };
        warn!(?ticket.member, class = ticket.class.label(), %why, "Misanswer, member benched");
        let class = ticket.class.label();
        let labels = [("member", member), ("class", class.to_owned())];
        metrics::counter!("zaino_traffic_misanswers_total", &labels).increment(1);
        self.shared.step(Input::Report(ticket));
    }

    /// Runs the sends the core names until it ends the ask
    async fn ask<T, E>(
        &self,
        class: Class,
        route: Route,
        send: impl Fn(MemberId) -> BoxFuture<'static, Result<T, QueryError<E>>>,
    ) -> Result<Answered<T>, Unanswered<E>>
    where
        E: fmt::Debug + fmt::Display,
    {
        let (mailbox, mut events) = mpsc::unbounded_channel();
        let mut open = Open {
            shared: &self.shared,
            ask: self.shared.open(mailbox, class, route),
            ended: false,
        };
        let mut sends = FuturesUnordered::new();
        let mut aborts: HashMap<MemberId, AbortHandle> = HashMap::new();
        let (mut value, mut failed, mut domain) = (None, None, None);
        loop {
            tokio::select! {
                biased;
                event = events.recv() => match event.expect("mailbox open while its ask runs") {
                    Event::Send(ticket) => {
                        let (reply, abort) = abortable(send(ticket.member));
                        aborts.insert(ticket.member, abort);
                        sends.push(reply.map(move |reply| (ticket, reply)));
                    }
                    Event::Cancel(ticket) => {
                        aborts.remove(&ticket.member).iter().for_each(AbortHandle::abort);
                    }
                    Event::Answered(ticket) => {
                        open.ended = true;
                        let value = value.take().expect("Answered follows the value just replied");
                        return Ok(Answered { value, from: ticket.member, ticket });
                    }
                    Event::Unanswered => {
                        open.ended = true;
                        let last = failed.take().map(QueryError::NonDomain);
                        let last = last.or(domain.take().map(QueryError::Domain));
                        return Err(Unanswered { last });
                    }
                },
                Some((ticket, Ok(reply))) = sends.next() => {
                    let reply = match reply {
                        Ok(answer) => {
                            value = Some(answer);
                            Reply::Value
                        }
                        Err(QueryError::Domain(said)) => {
                            domain = Some(said);
                            Reply::Domain
                        }
                        Err(QueryError::NonDomain(cause)) => {
                            failed.get_or_insert(cause);
                            Reply::NonDomain
                        }
                    };
                    self.shared.step(Input::Reply { ticket, reply });
                }
            }
        }
    }
}

/// Abandons its ask on drop unless the core ended it
struct Open<'a, S> {
    shared: &'a Shared<S>,
    ask: AskId,
    ended: bool,
}

impl<S> Drop for Open<'_, S> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.mailboxes.remove(&self.ask);
        if !self.ended {
            self.shared.step_locked(&mut state, Input::Abandon(self.ask));
        }
        drop(state);
        self.shared.changed.notify_one();
    }
}

impl<S> Shared<S> {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("traffic state mutex poisoned")
    }

    fn step(&self, input: Input) {
        let mut state = self.lock();
        self.step_locked(&mut state, input);
        drop(state);
        self.changed.notify_one();
    }

    /// Ask events to their futures (a dropped one's = ignored), polls to the driver
    fn step_locked(&self, state: &mut State, input: Input) {
        for output in state.core.step(input, Instant::now().into_std()) {
            let (ask, event) = match output {
                Output::Poll(order) => {
                    state.polls.push(order);
                    continue;
                }
                Output::Send(ticket) => (ticket.ask, Event::Send(ticket)),
                Output::Cancel(ticket) => (ticket.ask, Event::Cancel(ticket)),
                Output::Answered(ticket) => (ticket.ask, Event::Answered(ticket)),
                Output::Unanswered(ask) => (ask, Event::Unanswered),
            };
            if let Some(mailbox) = state.mailboxes.get(&ask) {
                let _ = mailbox.send(event);
            }
        }
    }

    fn open(&self, mailbox: mpsc::UnboundedSender<Event>, class: Class, route: Route) -> AskId {
        let mut state = self.lock();
        let ask = AskId(state.next_ask);
        state.next_ask += 1;
        state.mailboxes.insert(ask, mailbox);
        self.step_locked(&mut state, Input::Ask { ask, class, route });
        drop(state);
        self.changed.notify_one();
        ask
    }

    /// T2: the core sends trusted-only classes to trusted members
    fn trusted_only(&self, member: MemberId) -> Arc<S> {
        let MemberId::Trusted(validator) = member else {
            unreachable!("T2: a trusted-only class reached a peer");
        };
        Arc::clone(&self.trusted[validator.get()])
    }

    /// One send's future: the trusted source's, or the peer's (its `Err` = `NonDomain`)
    fn send<T, E, F>(
        &self,
        member: MemberId,
        trusted: impl FnOnce(Arc<S>) -> F,
        peer: impl FnOnce(&dyn PeerTransport, PeerId) -> BoxFuture<'static, Result<T, NonDomainError>>,
    ) -> BoxFuture<'static, Result<T, QueryError<E>>>
    where
        T: 'static,
        E: fmt::Debug + fmt::Display + 'static,
        F: Future<Output = Result<T, QueryError<E>>> + Send + 'static,
    {
        match member {
            MemberId::Trusted(validator) => {
                trusted(Arc::clone(&self.trusted[validator.get()])).boxed()
            }
            MemberId::Peer(id) => peer(self.peers(), id).map_err(QueryError::NonDomain).boxed(),
        }
    }

    fn peers(&self) -> &dyn PeerTransport {
        self.peers.as_deref().expect("peer members join through PeerTransport")
    }
}

impl<S: ChainDataSource> TrafficDriver<S> {
    /// Until `cancel`: steps the core at its wake time, runs its polls, follows peers
    pub async fn run(self, cancel: CancellationToken) {
        let shared = &self.shared;
        let mut membership = match &shared.peers {
            Some(peers) => peers.joined_left(),
            None => stream::pending().boxed(),
        };
        let mut polls = FuturesUnordered::new();
        shared.step(Input::Tick);
        loop {
            let (orders, table, wake) = {
                let mut state = shared.lock();
                (std::mem::take(&mut state.polls), state.core.table(), state.core.wake())
            };
            polls.extend(orders.into_iter().map(|order| shared.poll(order)));
            shared.table.send_replace(Arc::new(table));
            let timer = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(Instant::from_std(at)).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = shared.changed.notified() => {}
                () = timer => shared.step(Input::Tick),
                Some(polled) = polls.next() => shared.polled(polled),
                Some(change) = membership.next() => shared.step(match change {
                    Membership::Joined(peer) => Input::Joined(peer),
                    Membership::Left(peer) => Input::Left(peer),
                }),
            }
        }
    }
}

impl<S: ChainDataSource> Shared<S> {
    fn poll(&self, order: PollOrder) -> BoxFuture<'static, Polled> {
        let source = Arc::clone(&self.trusted[order.member.get()]);
        let asked = (self.best.lock().expect("poll best mutex poisoned"))();
        async move {
            let polled = source.get_poll_reading(order.metadata, asked.as_slice()).await;
            (order, asked, polled)
        }
        .boxed()
    }

    /// Mempool listed = `Live`, unlisted (inactive or none) = `CatchingUp`
    fn polled(&self, (order, asked, polled): Polled) {
        let read = polled.as_ref().ok().map(|reading| match reading.listing {
            Ok(_) => Synced::Live,
            Err(_) => Synced::CatchingUp,
        });
        let mut state = self.lock();
        self.step_locked(&mut state, Input::Polled { member: order.member, read });
        let health = state.core.health(order.member);
        drop(state);
        self.changed.notify_one();
        let PollOrder { member, streaming, .. } = order;
        let at = Instant::now();
        let observation = Observation { member, at, asked, polled, streaming, health };
        self.observations[member.get()].send_replace(Some(Arc::new(observation)));
    }
}
