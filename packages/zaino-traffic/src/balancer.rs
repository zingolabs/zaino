//! [`TrafficBalancer`]: the async face of the core (`usage.md`)
//!
//! - One ask = one future: registered, runs the sends the core names, feeds each reply back
//! - Dropping it abandons the ask (its sends dropped with it, permits back)
//! - [`TrafficDriver`]: the core's clock (hedges, retries, poll cadence), polls

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt, TryFutureExt};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zaino_primitives::types::Block;
use zaino_source::{
    BlockLink, BlockLinks, ChainDataSource, CheckAdmissionError, GetAtHeightError,
    GetRawMempoolTransactionError, GetTransactionError, MempoolListed, NonDomainError, PollReading,
    QueryError, RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, TxId};

use crate::class::Class;
use crate::core::{AskId, Input, Output, PollOrder, Push, Reply, Route, Ticket, TrafficCore};
use crate::member::{Health, Limits, MemberId, MemberTable, Synced, ValidatorId};

/// Block ask open this long without an answer → warn, and again each period after
const STALL_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

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

/// Trusted: `getblockheader` per height, one member; peers: `FindHeaders`, the peer set
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderAsk {
    Pinned { member: ValidatorId, heights: Vec<BlockHeight> },
    Peers { locator: Vec<BlockHash>, stop: Option<BlockHash> },
}

/// The p2p peer set as one member (zainod implements it over `zaino-peers`)
///
/// - no answer and "not found" alike = `Err` (a peer's absence proves nothing)
/// - a block or transaction not matching what was asked never comes back (the transport drops
///   it and blames the peer that sent it)
pub trait PeerTransport: Send + Sync + 'static {
    fn headers(
        &self,
        locator: Vec<BlockHash>,
        stop: Option<BlockHash>,
    ) -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>>;
    fn block(&self, hash: BlockHash) -> BoxFuture<'static, Result<Block, NonDomainError>>;
}

/// The peer set's member: `in_flight` = requests it carries at once (≥ [`MIN_IN_FLIGHT`])
///
/// [`MIN_IN_FLIGHT`]: Self::MIN_IN_FLIGHT
pub struct Peers {
    pub transport: Arc<dyn PeerTransport>,
    pub in_flight: u32,
}

impl Peers {
    /// One reserved per lane (control: headers, interactive: tip blocks, bulk: bulk blocks)
    pub const MIN_IN_FLIGHT: u32 = crate::class::LANES as u32;
}

/// One trusted member's latest poll (raw reading, latest only); `health` = right after it
///
/// - `asked` = where its `getblockhash` was asked ([`TrafficBalancer::poll_best`], read at start)
#[derive(Debug)]
pub struct Observation {
    pub asked: Option<BlockHeight>,
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
type PollBest = Box<dyn Fn() -> Option<BlockHeight> + Send + Sync>;

/// One finished poll: its order, the height asked, the reading
type Polled = (PollOrder, Option<BlockHeight>, Result<PollReading, NonDomainError>);

struct Shared<S> {
    state: Mutex<State>,
    trusted: Vec<Arc<S>>,
    peers: Option<Arc<dyn PeerTransport>>,
    best: OnceLock<PollBest>,
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
    Answered(Ticket),
    Unanswered,
}

impl<S: ChainDataSource> TrafficBalancer<S> {
    /// `trusted[i]` = `ValidatorId(i)` (non-empty, ≤ `ValidatorId::MAX`)
    pub fn new(trusted: Vec<Trusted<S>>, peers: Option<Peers>) -> (Self, TrafficDriver<S>) {
        let config: Vec<(u8, Limits)> = trusted.iter().map(|t| (t.priority, t.limits)).collect();
        let in_flight = peers.as_ref().map(|peers| peers.in_flight);
        let core = TrafficCore::new(&config, in_flight, Instant::now().into_std());
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
            peers: peers.map(|peers| peers.transport),
            best: OnceLock::new(),
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
                move |peers| peers.block(hash),
            )
        };
        self.block_by(urgency, Route::Any, send).await
    }

    /// Best-chain blocks at `heights`, one batched request to a trusted member (peers answer by
    /// hash only); pending until one serves them all; drop = abandon
    pub async fn blocks_at(
        &self,
        heights: Arc<[BlockHeight]>,
        urgency: Urgency,
    ) -> Answered<Vec<Block>> {
        let shared = Arc::clone(&self.shared);
        let send = move |member| {
            let (source, heights) = (shared.trusted_only(member), Arc::clone(&heights));
            async move { source.get_blocks_by_height(&heights).await }.boxed()
        };
        self.block_by(urgency, Route::Trusted, send).await
    }

    /// One block ask in `urgency`'s class (rounds retried, never unanswered)
    async fn block_by<T, E>(
        &self,
        urgency: Urgency,
        route: Route,
        send: impl Fn(MemberId) -> BoxFuture<'static, Result<T, QueryError<E>>>,
    ) -> Answered<T>
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
                    assert_eq!(to, MemberId::Peers, "Route::Peers reaches the peer set only");
                    let links = |headers: Vec<Vec<u8>>| {
                        headers.into_iter().map(|header| Ok(BlockLink { header })).collect()
                    };
                    let headers = shared.peers().headers(locator.clone(), stop);
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
            let (source, listed) = (shared.trusted_only(member), listed.clone());
            let raws = async move { source.get_raw_mempool_transactions(&listed).await };
            raws.map_err(QueryError::NonDomain).boxed()
        };
        self.ask(Class::Bytes, Route::Prefer(prefer), send).await
    }

    /// Mined or in a mempool: absent → next trusted member
    pub async fn transaction(
        &self,
        txid: TxId,
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
    ) -> Result<Answered<TxId>, Unanswered<SendRawTransactionError>> {
        self.on_alone(member, move |source| {
            let raw = raw.clone();
            async move { source.send_raw_transaction(raw).await }.boxed()
        })
        .await
    }

    /// `checkadmission` on `member` alone: the send's verdict, nothing relayed; one attempt
    pub async fn check_admission(
        &self,
        member: ValidatorId,
        raw: Vec<u8>,
    ) -> Result<Answered<TxId>, Unanswered<CheckAdmissionError>> {
        self.on_alone(member, move |source| {
            let raw = raw.clone();
            async move { source.check_admission(raw).await }.boxed()
        })
        .await
    }

    /// A `Submit`-class ask of trusted `member` alone
    async fn on_alone<T, E>(
        &self,
        member: ValidatorId,
        ask: impl Fn(Arc<S>) -> BoxFuture<'static, Result<T, QueryError<E>>>,
    ) -> Result<Answered<T>, Unanswered<E>>
    where
        E: fmt::Debug + fmt::Display,
    {
        let shared = Arc::clone(&self.shared);
        let send = move |to| ask(shared.trusted_only(to));
        self.ask(Class::Submit, Route::Only(MemberId::Trusted(member)), send).await
    }

    /// Trusted members a submission may enter by: live, not benched
    pub fn entries(&self) -> Vec<ValidatorId> {
        self.shared.lock().core.entries()
    }

    /// Where each poll asks `getblockhash`: `best()` as the poll starts (`None` = not asked)
    ///
    /// - never wakes a poll (an answer at a moved best = one stale fact, re-asked next poll)
    /// - set once (one chain view per balancer); unset = nothing asked
    pub fn poll_best(&self, best: impl Fn() -> Option<BlockHeight> + Send + Sync + 'static) {
        let set = self.shared.best.set(Box::new(best)).is_ok();
        assert!(set, "poll_best set twice: one chain view per balancer");
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

    /// `ticket`'s value failed the caller's check: a trusted sender benched, and only it (the peer
    /// set never: its transport already dropped what it could check)
    pub fn report(&self, ticket: Ticket, why: &(dyn std::error::Error + 'static)) {
        let member = match ticket.member {
            MemberId::Trusted(validator) => validator.get().to_string(),
            MemberId::Peers => "peers".to_owned(),
        };
        warn!(?ticket.member, class = ticket.class.label(), %why, "Misanswer");
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
        let (mut value, mut failed, mut domain) = (None, None, None);
        let opened = Instant::now();
        // rounds retry forever (no block skipped): a block no member serves = a silent stall unwarned
        let mut stall_warn_at = class.retries_rounds().then(|| opened + STALL_WARN_EVERY);
        let mut latest_failure: Option<(MemberId, String)> = None;
        let mut warned = false;
        loop {
            let stall_warning = async {
                match stall_warn_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                event = events.recv() => match event.expect("mailbox open while its ask runs") {
                    Event::Send(ticket) => {
                        sends.push(send(ticket.member).map(move |reply| (ticket, reply)));
                    }
                    Event::Answered(ticket) => {
                        open.ended = true;
                        let value = value.take().expect("Answered follows the value just replied");
                        if warned {
                            let (class, open_for) = (class.label(), opened.elapsed());
                            info!(class, ?open_for, from = ?ticket.member, "Stalled ask answered");
                        }
                        return Ok(Answered { value, ticket });
                    }
                    Event::Unanswered => {
                        open.ended = true;
                        let last = failed.take().map(QueryError::NonDomain);
                        let last = last.or(domain.take().map(QueryError::Domain));
                        return Err(Unanswered { last });
                    }
                },
                Some((ticket, reply)) = sends.next() => {
                    let reply = match reply {
                        Ok(answer) => {
                            value = Some(answer);
                            Reply::Value
                        }
                        Err(QueryError::Domain(said)) => {
                            latest_failure = Some((ticket.member, said.to_string()));
                            domain = Some(said);
                            Reply::Domain
                        }
                        Err(QueryError::NonDomain(cause)) => {
                            latest_failure = Some((ticket.member, cause.to_string()));
                            failed.get_or_insert(cause);
                            Reply::NonDomain
                        }
                    };
                    self.shared.step(Input::Reply { ticket, reply });
                }
                () = stall_warning => {
                    let (class, open_for) = (class.label(), opened.elapsed());
                    let (member, cause) = match &latest_failure {
                        Some((member, cause)) => (Some(member), cause.as_str()),
                        None => (None, "no reply yet"),
                    };
                    warn!(class, ?open_for, ?member, cause, "Ask unanswered, still retrying");
                    stall_warn_at = stall_warn_at.map(|at| at + STALL_WARN_EVERY);
                    warned = true;
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

    /// One send's future: the trusted source's, or the peer set's (its `Err` = `NonDomain`)
    fn send<T, E, F>(
        &self,
        member: MemberId,
        trusted: impl FnOnce(Arc<S>) -> F,
        peers: impl FnOnce(&dyn PeerTransport) -> BoxFuture<'static, Result<T, NonDomainError>>,
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
            MemberId::Peers => peers(self.peers()).map_err(QueryError::NonDomain).boxed(),
        }
    }

    fn peers(&self) -> &dyn PeerTransport {
        self.peers.as_deref().expect("the peer set member exists only with its transport")
    }
}

impl<S: ChainDataSource> TrafficDriver<S> {
    /// Until `cancel`: steps the core at its wake time, runs its polls
    pub async fn run(self, cancel: CancellationToken) {
        let shared = &self.shared;
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
            }
        }
    }
}

impl<S: ChainDataSource> Shared<S> {
    fn poll(&self, order: PollOrder) -> BoxFuture<'static, Polled> {
        let source = Arc::clone(&self.trusted[order.member.get()]);
        let asked = self.best.get().and_then(|best| best());
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
        let observation = Observation { asked, polled, streaming, health };
        self.observations[member.get()].send_replace(Some(Arc::new(observation)));
    }
}
