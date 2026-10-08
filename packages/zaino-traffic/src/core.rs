//! [`TrafficCore`]: who is asked what, and when (`traffic-balancer.md` §3, §4, §7)
//!
//! - Pure: no I/O, time + seeded RNG as inputs (sends, replies, polls = the driver's)
//! - One dispatch per step: due polls, then open asks by class priority
//! - Pick = eligible members with room → best tier (T6) → `Prefer`, else P2C on PeakEwma cost
//! - One policy: domain reply → next member; failure or hedge → next member out of the budget;
//!   every eligible member tried → retry the round (blocks) or unanswered

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::class::{Class, CLASSES};
use crate::limits::RetryBudget;
use crate::member::{
    Health, Limits, Member, MemberId, PeerId, Synced, ValidatorId, MIN_POLL_SPACING,
};

/// Block ask's round over → next round after this
const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AskId(pub(crate) u64);

/// One send of one ask: a misanswer report charges `member` and only it (T8)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket {
    pub(crate) ask: AskId,
    pub(crate) member: MemberId,
    pub(crate) class: Class,
}

/// Which members an ask may reach (beyond its class's kinds)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Any,
    Only(MemberId),
    Peers,
    /// Within the best tier with room: the first of these, else P2C
    Prefer(Vec<MemberId>),
}

impl Route {
    fn admits(&self, member: MemberId) -> bool {
        match self {
            Self::Any | Self::Prefer(_) => true,
            Self::Only(only) => *only == member,
            Self::Peers => matches!(member, MemberId::Peer(_)),
        }
    }
}

/// `QueryError`'s split: a value, the member's own answer (absent, rejected), no answer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reply {
    Value,
    Domain,
    NonDomain,
}

/// Push stream event of one trusted member
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Changed,
    Link(bool),
}

#[derive(Debug, Clone)]
pub(crate) enum Input {
    Ask {
        ask: AskId,
        class: Class,
        route: Route,
        cost: u32,
    },
    Reply {
        ticket: Ticket,
        reply: Reply,
    },
    Abandon(AskId),
    Report(Ticket),
    /// `read` = `None`: no answer
    Polled {
        member: ValidatorId,
        read: Option<Synced>,
    },
    Push {
        member: ValidatorId,
        push: Push,
    },
    Joined(PeerId),
    Left(PeerId),
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Output {
    Send(Ticket),
    /// Its peer left: drop that send (the ask moves on)
    Cancel(Ticket),
    /// Ends the ask: `ticket`'s value delivered, every other send dropped
    Answered(Ticket),
    Unanswered(AskId),
    Poll(PollOrder),
}

/// One poll batch: tip + listing + `getblockhash` (+ metadata)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PollOrder {
    pub(crate) member: ValidatorId,
    pub(crate) metadata: bool,
    pub(crate) streaming: bool,
}

pub(crate) struct TrafficCore {
    members: BTreeMap<MemberId, Member>,
    asks: BTreeMap<AskId, Ask>,
    budget: RetryBudget,
    rng: fastrand::Rng,
    stamp: u64,
    now: Instant,
    wake: Option<Instant>,
}

/// `tried` = members asked this round (in flight included), `failed` = next send is a retry
#[derive(Debug, Clone)]
struct Ask {
    class: Class,
    route: Route,
    cost: u32,
    sends: Vec<Sent>,
    tried: BTreeSet<MemberId>,
    failed: bool,
    retry_at: Option<Instant>,
}

/// `top` = best tier with room when picked (T6)
#[derive(Debug, Clone, Copy)]
struct Sent {
    member: MemberId,
    at: Instant,
    top: u16,
}

enum Next {
    Wait(Option<Instant>),
    Send { member: MemberId, top: u16, budgeted: bool },
    RoundOver,
}

impl TrafficCore {
    /// `trusted[i]` = `ValidatorId(i)`: (priority, limits)
    pub(crate) fn new(trusted: &[(u8, Limits)], seed: u64, now: Instant) -> Self {
        assert!(!trusted.is_empty(), "a trusted validator to ask");
        assert!(trusted.len() <= ValidatorId::MAX, "at most ValidatorId::MAX trusted validators");
        let members = trusted.iter().enumerate().filter_map(|(index, (priority, limits))| {
            let id = MemberId::Trusted(ValidatorId::new(index)?);
            Some((id, Member::trusted(*priority, *limits, now)))
        });
        Self {
            members: members.collect(),
            asks: BTreeMap::new(),
            budget: RetryBudget::new(now),
            rng: fastrand::Rng::with_seed(seed),
            stamp: 0,
            now,
            wake: None,
        }
    }

    /// Next instant a `Tick` acts (hedge, round retry, budget, rate, poll due)
    pub(crate) fn wake(&self) -> Option<Instant> {
        self.wake
    }

    pub(crate) fn step(&mut self, input: Input, now: Instant) -> Vec<Output> {
        assert!(now >= self.now, "time never runs back");
        self.now = now;
        let mut out = Vec::new();
        match input {
            Input::Ask { ask, class, route, cost } => {
                assert!(class != Class::Poll, "a poll is the core's own, never asked");
                assert!(!self.asks.contains_key(&ask), "a fresh ask id");
                let (sends, tried) = (Vec::new(), BTreeSet::new());
                let open = Ask { class, route, cost, sends, tried, failed: false, retry_at: None };
                self.asks.insert(ask, open);
            }
            Input::Reply { ticket, reply } => self.reply(ticket, reply, &mut out),
            Input::Abandon(ask) => {
                assert!(self.asks.contains_key(&ask), "abandon: an open ask");
                self.close(ask);
            }
            Input::Report(ticket) => {
                let stamp = self.next_stamp();
                if let Some(member) = self.members.get_mut(&ticket.member) {
                    member.bench(now, stamp);
                }
            }
            Input::Polled { member, read } => self.polled(member, read),
            Input::Push { member, push } => {
                let poller = self.members.get_mut(&MemberId::Trusted(member));
                let poller =
                    poller.and_then(|m| m.poller.as_mut()).expect("a configured validator");
                if let Push::Link(up) = push {
                    poller.streaming = up;
                }
                poller.wake = true;
            }
            Input::Joined(peer) => {
                let joined = self.members.insert(MemberId::Peer(peer), Member::peer(now));
                assert!(joined.is_none(), "a peer joins once");
            }
            Input::Left(peer) => self.left(peer, &mut out),
            Input::Tick => {}
        }
        self.dispatch(&mut out);
        #[cfg(debug_assertions)]
        self.check();
        out
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    fn reply(&mut self, ticket: Ticket, reply: Reply, out: &mut Vec<Output>) {
        let stamp = self.next_stamp();
        let ask = self.asks.get_mut(&ticket.ask);
        let sent = ask.and_then(|ask| {
            let sent = ask.sends.iter().position(|sent| sent.member == ticket.member)?;
            ask.failed |= reply == Reply::NonDomain;
            Some(ask.sends.remove(sent))
        });
        let sent = sent.expect("a reply for a send in flight");
        let member =
            self.members.get_mut(&ticket.member).expect("T4: no send to a departed member");
        member.in_flight[ticket.class.index()] -= 1;
        let elapsed = self.now.saturating_duration_since(sent.at);
        member.latency.observe(elapsed, self.now);
        let answered = reply != Reply::NonDomain;
        if answered {
            member.answers[ticket.class.index()].push(elapsed);
        }
        member.outcome(answered, stamp);
        if reply == Reply::Value {
            self.close(ticket.ask);
            out.push(Output::Answered(ticket));
        }
    }

    /// Ask gone, its permits back
    fn close(&mut self, id: AskId) {
        let ask = self.asks.remove(&id).expect("closing an open ask");
        for sent in ask.sends {
            if let Some(member) = self.members.get_mut(&sent.member) {
                member.in_flight[ask.class.index()] -= 1;
            }
        }
    }

    fn polled(&mut self, id: ValidatorId, read: Option<Synced>) {
        let stamp = self.next_stamp();
        let member = self.members.get_mut(&MemberId::Trusted(id));
        let member = member.filter(|m| m.poller.as_ref().is_some_and(|p| p.in_flight));
        let member = member.expect("a poll result for a poll in flight");
        let poller = member.poller.as_mut().expect("trusted members poll");
        poller.finished(read.is_some());
        let started = poller.last.expect("a poll in flight started");
        member.in_flight[Class::Poll.index()] -= 1;
        member.latency.observe(self.now.saturating_duration_since(started), self.now);
        member.outcome(read.is_some(), stamp);
        member.synced = read.or(member.synced);
    }

    /// Its sends dropped, each ask moving on as after a failure
    fn left(&mut self, peer: PeerId, out: &mut Vec<Output>) {
        let gone = MemberId::Peer(peer);
        assert!(self.members.remove(&gone).is_some(), "a peer leaves once, after joining");
        for (id, ask) in &mut self.asks {
            if let Some(sent) = ask.sends.iter().position(|sent| sent.member == gone) {
                ask.sends.remove(sent);
                ask.failed = true;
                out.push(Output::Cancel(Ticket { ask: *id, member: gone, class: ask.class }));
            }
        }
    }

    fn dispatch(&mut self, out: &mut Vec<Output>) {
        self.wake = None;
        self.poll(out);
        let mut order: Vec<(Class, AskId)> =
            self.asks.iter().map(|(id, ask)| (ask.class, *id)).collect();
        order.sort_unstable();
        for (_, id) in order {
            self.advance(id, out);
        }
    }

    /// Every due poll sent (T10: charged to the rate, never refused by it)
    fn poll(&mut self, out: &mut Vec<Output>) {
        let now = self.now;
        for (id, member) in &mut self.members {
            let (MemberId::Trusted(validator), Some(poller)) = (id, member.poller.as_mut()) else {
                continue;
            };
            if poller.in_flight {
                continue;
            }
            let due = poller.due(member.failures, now);
            if due > now {
                earliest(&mut self.wake, due);
                continue;
            }
            let metadata = poller.start(now);
            member.in_flight[Class::Poll.index()] += 1;
            let cost = 3 + if metadata { 3 } else { 0 };
            member.rate.iter_mut().for_each(|rate| rate.charge(now, cost));
            let streaming = poller.streaming;
            out.push(Output::Poll(PollOrder { member: *validator, metadata, streaming }));
        }
    }

    fn advance(&mut self, id: AskId, out: &mut Vec<Output>) {
        let now = self.now;
        let ask = self.asks.get_mut(&id).expect("dispatch walks open asks");
        match ask.retry_at {
            Some(at) if at > now => return earliest(&mut self.wake, at),
            Some(_) => (ask.retry_at, ask.failed) = (None, false),
            None => {}
        }
        match self.next(id) {
            Next::Wait(at) => at.into_iter().for_each(|at| earliest(&mut self.wake, at)),
            Next::Send { member, top, budgeted } => self.send(id, member, top, budgeted, out),
            Next::RoundOver => {
                let ask = self.asks.get_mut(&id).expect("dispatch walks open asks");
                if ask.class.retries_rounds() {
                    ask.tried.clear();
                    ask.retry_at = Some(now + RETRY);
                    earliest(&mut self.wake, now + RETRY);
                } else {
                    self.close(id);
                    out.push(Output::Unanswered(id));
                }
            }
        }
    }

    /// Hedge due → eligible → budget → room → pick
    fn next(&mut self, id: AskId) -> Next {
        let now = self.now;
        let ask = &self.asks[&id];
        let hedge = !ask.sends.is_empty();
        if hedge {
            match self.hedge_due(ask) {
                Some(due) if due <= now => {}
                due => return Next::Wait(due),
            }
        }
        let eligible: Vec<(&MemberId, &Member)> =
            self.members.iter().filter(|(id, member)| eligible(ask, **id, member, now)).collect();
        if eligible.is_empty() {
            return if hedge { Next::Wait(None) } else { Next::RoundOver };
        }
        let budgeted = hedge || ask.failed;
        if budgeted && !self.budget.available(now) {
            return Next::Wait(Some(self.budget.ready_at(now)));
        }
        let roomy: Vec<(&MemberId, &Member)> =
            eligible.iter().copied().filter(|(_, m)| m.room(ask.class, now)).collect();
        let Some(top) = roomy.iter().map(|(_, member)| member.tier).min() else {
            let rate_bound =
                eligible.iter().filter(|(_, m)| m.permits.admits(&m.in_flight, ask.class));
            let ready =
                rate_bound.filter_map(|(_, m)| m.rate.as_ref().map(|rate| rate.ready_at(now)));
            return Next::Wait(ready.min());
        };
        let best: Vec<MemberId> =
            roomy.iter().filter(|(_, m)| m.tier == top).map(|(id, _)| **id).collect();
        let preferred = match &ask.route {
            Route::Prefer(prefer) => prefer.iter().find(|id| best.contains(id)).copied(),
            _ => None,
        };
        let member = preferred.unwrap_or_else(|| p2c(&best, &self.members, &mut self.rng, now));
        Next::Send { member, top, budgeted }
    }

    /// Every send past its member's hedge delay (`None` = class never hedged)
    fn hedge_due(&self, ask: &Ask) -> Option<Instant> {
        let due = ask.sends.iter().map(|sent| {
            let after = self.members[&sent.member].hedge_after(ask.class)?;
            Some(sent.at + after)
        });
        due.collect::<Option<Vec<Instant>>>()?.into_iter().max()
    }

    fn send(
        &mut self,
        id: AskId,
        member: MemberId,
        top: u16,
        budgeted: bool,
        out: &mut Vec<Output>,
    ) {
        let (now, stamp) = (self.now, self.next_stamp());
        let ask = self.asks.get_mut(&id).expect("dispatch walks open asks");
        match (budgeted, ask.tried.is_empty()) {
            (true, _) => self.budget.withdraw(now),
            (false, true) => self.budget.deposit(now),
            (false, false) => {}
        }
        let to = self.members.get_mut(&member).expect("picked among members");
        to.in_flight[ask.class.index()] += 1;
        to.rate.iter_mut().for_each(|rate| rate.charge(now, ask.cost));
        to.last_sent = Some(stamp);
        ask.tried.insert(member);
        ask.sends.push(Sent { member, at: now, top });
        out.push(Output::Send(Ticket { ask: id, member, class: ask.class }));
    }

    /// Submit's sample space: trusted, live, not benched
    pub(crate) fn entries(&self) -> Vec<ValidatorId> {
        let live = self
            .members
            .iter()
            .filter(|(_, member)| member.health() == Health::Live && !member.benched(self.now));
        live.filter_map(|(id, _)| match id {
            MemberId::Trusted(validator) => Some(*validator),
            MemberId::Peer(_) => None,
        })
        .collect()
    }

    pub(crate) fn health(&self, member: ValidatorId) -> Health {
        let member = self.members.get(&MemberId::Trusted(member));
        member.expect("a configured validator").health()
    }

    pub(crate) fn table(&self) -> crate::MemberTable {
        let rows = self.members.iter().map(|(id, member)| crate::MemberRow {
            id: *id,
            health: member.health(),
            failures: member.failures,
            benched_until: member.bench.filter(|_| member.benched(self.now)).map(|b| b.until),
            latency: Duration::from_nanos(member.latency.estimate(self.now) as u64),
            in_flight: member.in_flight.iter().sum(),
        });
        crate::MemberTable { rows: rows.collect() }
    }

    /// T1–T8, T10 (T9 = the model's, at quiescence)
    pub(crate) fn check(&self) {
        for (id, member) in &self.members {
            assert!(
                member.permits.holds(&member.in_flight),
                "T1: in flight within max_connections and ceilings, reserves never borrowed"
            );
            let mut counted = [0u32; CLASSES];
            for ask in self.asks.values() {
                counted[ask.class.index()] +=
                    ask.sends.iter().filter(|s| s.member == *id).count() as u32;
            }
            counted[Class::Poll.index()] +=
                u32::from(member.poller.as_ref().is_some_and(|p| p.in_flight));
            assert_eq!(member.in_flight, counted, "T4: in flight = open asks' sends + polls");
            let benched = member.bench.filter(|_| member.benched(self.now));
            assert!(
                benched.is_none_or(|bench| member.last_sent < Some(bench.stamp)),
                "T3: no send to a benched member"
            );
            assert!(
                member.down_since.is_none_or(|down| member.last_sent < Some(down)),
                "T3: no send to a down member"
            );
            if let Some(poller) = &member.poller {
                let spaced = poller.previous.zip(poller.last);
                assert!(
                    spaced.is_none_or(|(previous, last)| last >= previous + MIN_POLL_SPACING),
                    "T10: polls of one member at least 200 ms apart"
                );
                assert!(
                    poller.in_flight || poller.due(member.failures, self.now) > self.now,
                    "T10: a due poll is sent"
                );
            }
        }
        for ask in self.asks.values() {
            let mut asked = BTreeSet::new();
            for sent in &ask.sends {
                let member =
                    self.members.get(&sent.member).expect("T4: no send to a departed member");
                let kind = matches!(sent.member, MemberId::Trusted(_)) || ask.class.peers();
                assert!(
                    kind && ask.route.admits(sent.member),
                    "T2: a class reaches only the member kinds its row allows"
                );
                assert!(
                    asked.insert(sent.member) && ask.tried.contains(&sent.member),
                    "T5: a round asks each member once"
                );
                assert_eq!(member.tier, sent.top, "T6: a send goes to the best tier with room");
            }
        }
        assert!(self.budget.holds(), "T7: retries and hedges within the budget");
    }
}

/// Route, kind, not benched, not down, synced for mempool classes, not tried this round
///
/// - synced read from the last poll, not `health()` (failing + catching up = `Degraded`)
fn eligible(ask: &Ask, id: MemberId, member: &Member, now: Instant) -> bool {
    let kind = matches!(id, MemberId::Trusted(_)) || ask.class.peers();
    let synced = !(ask.class.needs_synced() && member.synced == Some(Synced::CatchingUp));
    kind && ask.route.admits(id)
        && !member.benched(now)
        && member.health() != Health::Down
        && synced
        && !ask.tried.contains(&id)
}

/// Cheaper of two at random (tower `p2c`)
fn p2c(
    best: &[MemberId],
    members: &BTreeMap<MemberId, Member>,
    rng: &mut fastrand::Rng,
    now: Instant,
) -> MemberId {
    let n = best.len();
    let a = rng.usize(..n);
    if n == 1 {
        return best[a];
    }
    let b = (a + 1 + rng.usize(..n - 1)) % n;
    let cost = |i: usize| members[&best[i]].cost(now);
    if cost(b) < cost(a) {
        best[b]
    } else {
        best[a]
    }
}

fn earliest(wake: &mut Option<Instant>, at: Instant) {
    *wake = Some(wake.map_or(at, |wake| wake.min(at)));
}

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model;
