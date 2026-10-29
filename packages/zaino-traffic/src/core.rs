//! [`TrafficCore`]: who is asked what, and when (`usage.md`)
//!
//! - Pure: no I/O, time as input (sends, replies, polls = the driver's)
//! - One dispatch per step: due polls (own permit), then open asks by lane (control →
//!   interactive → bulk), FIFO
//! - Pick = eligible members with room → best tier (T6) → `Prefer`, else least in flight
//! - One policy
//!   - domain reply or failure → next member
//!   - silent past the hedge floor → a second member
//!   - every eligible member tried → retry the round (blocks) or unanswered

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::class::{Class, Lane};
use crate::member::{Health, Limits, Member, MemberId, Synced, TrustedValidatorId};

/// Block ask's round over → next round after this
const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AskId(pub(crate) u64);

/// One send of one ask: a misanswer report charges `member` and only it (T8)
///
/// - peer set → charges no one (its transport blames the peer)
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
    /// Trusted members only (a question peers cannot answer: a block by height)
    Trusted,
    /// Within the best tier with room: the first of these, else the least loaded
    Prefer(Vec<MemberId>),
}

impl Route {
    fn admits(&self, member: MemberId) -> bool {
        match self {
            Self::Any | Self::Prefer(_) => true,
            Self::Only(only) => *only == member,
            Self::Peers => member == MemberId::Peers,
            Self::Trusted => matches!(member, MemberId::TrustedValidator(_)),
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
    },
    Reply {
        ticket: Ticket,
        reply: Reply,
    },
    Abandon(AskId),
    Report(Ticket),
    /// `read` = `None`: no answer
    Polled {
        validator: TrustedValidatorId,
        read: Option<Synced>,
    },
    Push {
        validator: TrustedValidatorId,
        push: Push,
    },
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Output {
    Send(Ticket),
    /// Ends the ask: `ticket`'s value delivered, every other send dropped
    Answered(Ticket),
    Unanswered(AskId),
    Poll(PollOrder),
}

/// One poll batch: tip + listing + `getblockhash` (+ metadata)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PollOrder {
    pub(crate) validator: TrustedValidatorId,
    pub(crate) metadata: bool,
    pub(crate) streaming: bool,
}

pub(crate) struct TrafficCore {
    members: BTreeMap<MemberId, Member>,
    asks: BTreeMap<AskId, Ask>,
    now: Instant,
    wake: Option<Instant>,
}

/// `tried` = members asked this round (in flight included)
#[derive(Debug, Clone)]
struct Ask {
    class: Class,
    route: Route,
    sends: Vec<Sent>,
    tried: BTreeSet<MemberId>,
    retry_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Sent {
    member: MemberId,
    at: Instant,
}

enum Next {
    Wait(Option<Instant>),
    Send(MemberId),
    RoundOver,
}

impl TrafficCore {
    /// - `trusted[i]` = `TrustedValidatorId(i)`: (priority, limits)
    /// - `peers` = the peer set's in flight
    pub(crate) fn new(trusted: &[(u8, Limits)], peers: Option<u32>, now: Instant) -> Self {
        assert!(!trusted.is_empty(), "a trusted validator to ask");
        assert!(
            trusted.len() <= TrustedValidatorId::MAX,
            "at most TrustedValidatorId::MAX trusted validators"
        );
        let trusted = trusted.iter().enumerate().filter_map(|(index, (priority, limits))| {
            let id = MemberId::TrustedValidator(TrustedValidatorId::new(index)?);
            Some((id, Member::trusted(*priority, *limits)))
        });
        let peers = peers.map(|in_flight| (MemberId::Peers, Member::peers(in_flight)));
        let members = trusted.chain(peers).collect();
        Self { members, asks: BTreeMap::new(), now, wake: None }
    }

    /// Next instant a `Tick` acts (hedge, round retry, poll due)
    pub(crate) fn wake(&self) -> Option<Instant> {
        self.wake
    }

    pub(crate) fn step(&mut self, input: Input, now: Instant) -> Vec<Output> {
        assert!(now >= self.now, "time never runs back");
        self.now = now;
        let mut out = Vec::new();
        match input {
            Input::Ask { ask, class, route } => {
                assert!(!self.asks.contains_key(&ask), "a fresh ask id");
                let (sends, tried) = (Vec::new(), BTreeSet::new());
                self.asks.insert(ask, Ask { class, route, sends, tried, retry_at: None });
            }
            Input::Reply { ticket, reply } => self.reply(ticket, reply, &mut out),
            Input::Abandon(ask) => {
                assert!(self.asks.contains_key(&ask), "abandon: an open ask");
                self.close(ask);
            }
            // the peer set never benched: one peer's lie would starve every honest one
            Input::Report(ticket) => {
                if let MemberId::TrustedValidator(_) = ticket.member {
                    let member = self.members.get_mut(&ticket.member);
                    member.expect("report: a configured validator").bench(now);
                }
            }
            Input::Polled { validator, read } => self.polled(validator, read),
            Input::Push { validator, push } => {
                let poller = self.members.get_mut(&MemberId::TrustedValidator(validator));
                let poller =
                    poller.and_then(|m| m.poller.as_mut()).expect("push: a configured validator");
                if let Push::Link(up) = push {
                    poller.streaming = up;
                }
                poller.wake = true;
            }
            Input::Tick => {}
        }
        self.dispatch(&mut out);
        out
    }

    fn reply(&mut self, ticket: Ticket, reply: Reply, out: &mut Vec<Output>) {
        let ask = self.asks.get_mut(&ticket.ask);
        let sent = ask.and_then(|ask| {
            let sent = ask.sends.iter().position(|sent| sent.member == ticket.member)?;
            Some(ask.sends.remove(sent))
        });
        sent.expect("a reply for a send in flight");
        let member = self
            .members
            .get_mut(&ticket.member)
            .expect("member registered while its send is in flight");
        member.in_flight[ticket.class.lane().index()] -= 1;
        member.outcome(reply != Reply::NonDomain);
        if reply == Reply::Value {
            self.close(ticket.ask);
            out.push(Output::Answered(ticket));
        }
    }

    /// Ask gone, its permits back
    fn close(&mut self, id: AskId) {
        let ask = self.asks.remove(&id).expect("closing an open ask");
        for sent in ask.sends {
            let member = self
                .members
                .get_mut(&sent.member)
                .expect("member registered while its send is in flight");
            member.in_flight[ask.class.lane().index()] -= 1;
        }
    }

    fn polled(&mut self, id: TrustedValidatorId, read: Option<Synced>) {
        let member = self.members.get_mut(&MemberId::TrustedValidator(id));
        let member = member.filter(|m| m.poller.as_ref().is_some_and(|p| p.in_flight));
        let member = member.expect("a poll result for a poll in flight");
        let poller = member.poller.as_mut().expect("trusted members poll");
        poller.finished(read.is_some());
        member.outcome(read.is_some());
        member.synced = read.or(member.synced);
    }

    fn dispatch(&mut self, out: &mut Vec<Output>) {
        self.wake = None;
        self.poll(out);
        let mut order: Vec<(Lane, AskId)> =
            self.asks.iter().map(|(id, ask)| (ask.class.lane(), *id)).collect();
        order.sort_unstable();
        for (_, id) in order {
            self.advance(id, out);
        }
    }

    /// Every due poll sent (its own permit, outside every lane)
    fn poll(&mut self, out: &mut Vec<Output>) {
        let now = self.now;
        for (id, member) in &mut self.members {
            let (MemberId::TrustedValidator(validator), Some(poller)) =
                (id, member.poller.as_mut())
            else {
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
            let streaming = poller.streaming;
            out.push(Output::Poll(PollOrder { validator: *validator, metadata, streaming }));
        }
    }

    fn advance(&mut self, id: AskId, out: &mut Vec<Output>) {
        let now = self.now;
        let ask = self.asks.get_mut(&id).expect("dispatch walks open asks");
        match ask.retry_at {
            Some(at) if at > now => return earliest(&mut self.wake, at),
            Some(_) => ask.retry_at = None,
            None => {}
        }
        match self.next(id) {
            Next::Wait(at) => at.into_iter().for_each(|at| earliest(&mut self.wake, at)),
            Next::Send(member) => self.send(id, member, out),
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

    /// Hedge due → eligible → room → pick (`Wait(None)` = a completion steps again)
    fn next(&self, id: AskId) -> Next {
        let now = self.now;
        let ask = &self.asks[&id];
        let latest = ask.sends.iter().map(|sent| sent.at).max();
        if let Some(latest) = latest {
            match ask.class.hedge_floor().map(|floor| latest + floor) {
                Some(due) if due <= now => {}
                due => return Next::Wait(due),
            }
        }
        let eligible: Vec<(&MemberId, &Member)> =
            self.members.iter().filter(|(id, member)| eligible(ask, **id, member, now)).collect();
        if eligible.is_empty() {
            return if latest.is_some() { Next::Wait(None) } else { Next::RoundOver };
        }
        let lane = ask.class.lane();
        let roomy: Vec<(&MemberId, &Member)> =
            eligible.into_iter().filter(|(_, member)| member.room(lane)).collect();
        let least = roomy.iter().min_by_key(|(_, member)| (member.tier, member.load()));
        let Some((&least, best)) = least.copied() else { return Next::Wait(None) };
        let top = best.tier;
        let preferred = match &ask.route {
            Route::Prefer(prefer) => prefer
                .iter()
                .find(|id| roomy.iter().any(|(member, m)| *member == *id && m.tier == top))
                .copied(),
            _ => None,
        };
        Next::Send(preferred.unwrap_or(least))
    }

    fn send(&mut self, id: AskId, member: MemberId, out: &mut Vec<Output>) {
        let now = self.now;
        let ask = self.asks.get_mut(&id).expect("dispatch walks open asks");
        let to = self.members.get_mut(&member).expect("picked among members");
        to.in_flight[ask.class.lane().index()] += 1;
        ask.tried.insert(member);
        ask.sends.push(Sent { member, at: now });
        out.push(Output::Send(Ticket { ask: id, member, class: ask.class }));
    }

    /// Submit's sample space: trusted, live, not benched
    pub(crate) fn entries(&self) -> Vec<TrustedValidatorId> {
        let live = self
            .members
            .iter()
            .filter(|(_, member)| member.health() == Health::Live && !member.benched(self.now));
        live.filter_map(|(id, _)| match id {
            MemberId::TrustedValidator(validator) => Some(*validator),
            MemberId::Peers => None,
        })
        .collect()
    }

    pub(crate) fn health(&self, validator: TrustedValidatorId) -> Health {
        let member = self.members.get(&MemberId::TrustedValidator(validator));
        member.expect("health: a configured validator").health()
    }

    /// Each trusted member's health, configured order
    pub(crate) fn healths(&self) -> Vec<Health> {
        let trusted = self.members.iter().filter(|(id, _)| *id != &MemberId::Peers);
        trusted.map(|(_, member)| member.health()).collect()
    }

    /// T1, T2, T4, T5, T10 (the rest = the model's)
    #[cfg(test)]
    pub(crate) fn check(&self) {
        use crate::class::LANES;
        use crate::member::MIN_POLL_SPACING;

        for (id, member) in &self.members {
            let (in_flight, permits) = (member.in_flight, &member.permits);
            assert!(
                permits.holds(&in_flight),
                "in flight within max_connections, reserves never borrowed (T1): {id:?} in flight \
                 {in_flight:?} (control, interactive, bulk) over {permits:?}"
            );
            let mut counted = [0u32; LANES];
            for ask in self.asks.values() {
                let sends = ask.sends.iter().filter(|s| s.member == *id).count();
                counted[ask.class.lane().index()] +=
                    u32::try_from(sends).expect("sends per ask <= members");
            }
            assert_eq!(
                in_flight, counted,
                "in flight = open asks' sends (T4): {id:?} recorded vs counted per lane"
            );
            if let Some(poller) = &member.poller {
                if let Some((previous, last)) = poller.previous.zip(poller.last) {
                    assert!(
                        last >= previous + MIN_POLL_SPACING,
                        "polls of one member at least 200 ms apart (T10): {id:?} polled {:?} apart",
                        last - previous
                    );
                }
                let due = poller.due(member.failures, self.now);
                assert!(
                    poller.in_flight || due > self.now,
                    "a due poll is sent (T10): {id:?} due {:?} ago, none in flight",
                    self.now - due
                );
            }
        }
        for (ask_id, ask) in &self.asks {
            let (class, route) = (ask.class, &ask.route);
            let mut asked = BTreeSet::new();
            for sent in &ask.sends {
                let member = sent.member;
                assert!(
                    self.members.contains_key(&member),
                    "a send's member configured: {ask_id:?} {class:?} sent to {member:?}"
                );
                assert!(
                    matches!(member, MemberId::TrustedValidator(_)) || class.peers(),
                    "a trusted-only class never reaches the peer set (T2): {ask_id:?} {class:?} \
                     sent to {member:?}"
                );
                assert!(
                    route.admits(member),
                    "a send stays within its ask's route (T2): {ask_id:?} {class:?} via \
                     {route:?} sent to {member:?}"
                );
                assert!(
                    asked.insert(member),
                    "a round sends to each member once (T5): {ask_id:?} {class:?} sent to \
                     {member:?} twice"
                );
                assert!(
                    ask.tried.contains(&member),
                    "every send's member in its round's tried (T5): {ask_id:?} {class:?} sent \
                     to {member:?}, tried {:?} (a re-ask could reach it)",
                    ask.tried
                );
            }
        }
    }
}

/// Route, kind, not benched, not down, synced for mempool classes, not tried this round
///
/// - synced read from the last poll, not `health()` (failing + catching up = `Degraded`)
fn eligible(ask: &Ask, id: MemberId, member: &Member, now: Instant) -> bool {
    let kind = matches!(id, MemberId::TrustedValidator(_)) || ask.class.peers();
    let synced = !(ask.class.needs_synced() && member.synced == Some(Synced::CatchingUp));
    kind && ask.route.admits(id)
        && !member.benched(now)
        && member.health() != Health::Down
        && synced
        && !ask.tried.contains(&id)
}

fn earliest(wake: &mut Option<Instant>, at: Instant) {
    *wake = Some(wake.map_or(at, |wake| wake.min(at)));
}

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model_tests;
