//! [`TrafficCore`] against simulated members and a naive oracle (`traffic-balancer.md` §8)
//!
//! - Trusted 1..=4 (priorities 0..=2, 4 connections) + peers joining / leaving, one [`Kind`] each
//! - Kinds: slow (past every hedge floor), lying (a wrong value), lagging (absent; polls catching
//!   up), flapping (fails in odd 20 s phases, lagging's polls too), silent (transport timeout)
//! - Swarm: whole kinds off per case (an off one plays honest)
//! - Model = the caller: a lie answered → `report` (+ a block re-asked)
//! - Own lanes, permits and rounds (not the core's): the oracle's ground truth
//! - Per send: T2 kind + route, T3 bench + down, synced, T5 once per round, T6 pick, T7 priority
//! - After every step: T7 no ready ask waits beside room; at quiescence (10 min): T9, liars
//!   benched, T10 poll gaps ≤ the ladder's ceiling + one control holder's completion

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::{AskId, Input, Output, PollOrder, Push, Reply, Route, Ticket, TrafficCore};
use crate::class::Class;
use crate::member::{
    Limits, MemberId, PeerId, Synced, ValidatorId, DOWN_AFTER, LADDER_CEILING, MIN_POLL_SPACING,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Honest,
    Slow,
    Lying,
    Lagging,
    Flapping,
    Down,
    Silent,
}

const KINDS: [Kind; 7] = [
    Kind::Honest,
    Kind::Slow,
    Kind::Lying,
    Kind::Lagging,
    Kind::Flapping,
    Kind::Down,
    Kind::Silent,
];
const FAST: Duration = Duration::from_millis(50);
const SLOW: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(25);
const SETTLE: Duration = Duration::from_secs(600);
const ROUND_RETRY: Duration = Duration::from_secs(1);
const CONNECTIONS: u32 = 4;
const ASKED: [Class; 6] =
    [Class::Submit, Class::TipBlock, Class::Headers, Class::Lookup, Class::Bytes, Class::BulkBlock];

/// Control, interactive, bulk (dispatch order)
fn lane(class: Class) -> usize {
    match class {
        Class::Poll | Class::Submit | Class::Headers => 0,
        Class::TipBlock | Class::Lookup | Class::Bytes => 1,
        Class::BulkBlock => 2,
    }
}

#[derive(Debug, Clone)]
enum Move {
    Ask { class: u8, pick: u8 },
    Abandon(u8),
    Advance(u16),
    Join(Kind),
    Leave(u8),
    Push { member: u8, push: Push },
}

fn kind() -> impl Strategy<Value = Kind> {
    (0..KINDS.len()).prop_map(|kind| KINDS[kind])
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let push = prop_oneof![Just(Push::Changed), any::<bool>().prop_map(Push::Link)];
    let one = prop_oneof![
        8 => (any::<u8>(), any::<u8>()).prop_map(|(class, pick)| Move::Ask { class, pick }),
        2 => any::<u8>().prop_map(Move::Abandon),
        8 => (0u16..=20_000).prop_map(Move::Advance),
        2 => kind().prop_map(Move::Join),
        1 => any::<u8>().prop_map(Move::Leave),
        1 => (any::<u8>(), push).prop_map(|(member, push)| Move::Push { member, push }),
    ];
    prop::collection::vec(one, 1..64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// - `check()` + T7 after every step (T1–T6, T10), the oracle after every send
    /// - Quiescence: T9, liars benched, T10 gaps
    #[test]
    fn every_ask_an_honest_member_may_serve_is_served_and_no_send_breaks_the_rules(
        trusted in prop::collection::vec((kind(), 0u8..=2), 1..=4),
        moves in moves(),
        swarm in prop::array::uniform7(prop::bool::weighted(0.8)),
    ) {
        let on = |kind: Kind| if swarm[kind as usize] { kind } else { Kind::Honest };
        let trusted: Vec<(Kind, u8)> = trusted.into_iter().map(|(kind, p)| (on(kind), p)).collect();
        let mut world = World::new(&trusted);
        for one in &moves {
            world.play(one, &on);
        }
        world.settle();
    }
}

/// Tier 0 stalls, fails or lacks it, tier 1 answers in 50 ms: each ask ends at its bound
#[test]
fn an_ask_ends_within_its_hedge_floor_plus_one_round() {
    let ms = Duration::from_millis;
    let [v0, v1] = [0, 1].map(|i| MemberId::Trusted(ValidatorId::new(i).expect("small")));
    let cases = [
        (Kind::Silent, Class::TipBlock, Route::Any, ms(2_050), Some(v1)),
        (Kind::Slow, Class::TipBlock, Route::Any, ms(2_050), Some(v1)),
        (Kind::Silent, Class::Lookup, Route::Any, ms(1_050), Some(v1)),
        (Kind::Silent, Class::BulkBlock, Route::Any, ms(15_050), Some(v1)),
        (Kind::Lagging, Class::Lookup, Route::Any, ms(100), Some(v1)),
        (Kind::Down, Class::Bytes, Route::Prefer(Vec::new()), ms(100), Some(v1)),
        (Kind::Silent, Class::Headers, Route::Only(v0), TIMEOUT, None),
        (Kind::Down, Class::Submit, Route::Only(v0), FAST, None),
    ];
    for (kind, class, route, after, by) in cases {
        let mut world = World::new(&[(kind, 0), (Kind::Honest, 1)]);
        let asked_at = world.now;
        let ask = world.ask(class, route.clone());
        world.advance(asked_at + Duration::from_secs(60));
        let ended = world.ends.get(&ask).copied();
        assert_eq!(ended, Some((asked_at + after, by)), "{kind:?} tier 0, {class:?} {route:?}");
    }
}

struct World {
    core: TrafficCore,
    t0: Instant,
    now: Instant,
    kinds: BTreeMap<MemberId, Kind>,
    tiers: BTreeMap<MemberId, u16>,
    next_peer: u64,
    next_ask: u64,
    asks: BTreeMap<AskId, Asked>,
    ends: BTreeMap<AskId, (Instant, Option<MemberId>)>,
    pending: Vec<(Instant, Input)>,
    flying: BTreeMap<MemberId, [u32; 3]>,
    benched: BTreeMap<MemberId, (Instant, u32)>,
    failures: BTreeMap<MemberId, u32>,
    synced: BTreeMap<MemberId, Synced>,
    polls: BTreeMap<ValidatorId, Vec<Instant>>,
    answered_lies: BTreeSet<MemberId>,
    lies: Vec<(Ticket, Asked)>,
    value_from: Option<Ticket>,
}

/// `round` = members sent to this round, `flying` = sends in flight (sent at)
#[derive(Debug, Clone)]
struct Asked {
    class: Class,
    route: Route,
    round: BTreeSet<MemberId>,
    flying: BTreeMap<MemberId, Instant>,
    retry_at: Option<Instant>,
}

impl World {
    fn new(trusted: &[(Kind, u8)]) -> Self {
        let limits = Limits::new(CONNECTIONS, None).expect("MIN_CONNECTIONS");
        let config: Vec<(u8, Limits)> = trusted.iter().map(|(_, p)| (*p, limits)).collect();
        let t0 = Instant::now();
        let ids = (0..trusted.len()).map(|i| MemberId::Trusted(ValidatorId::new(i).expect("≤ 4")));
        let mut world = Self {
            core: TrafficCore::new(&config, t0),
            t0,
            now: t0,
            kinds: ids.clone().zip(trusted.iter().map(|(kind, _)| *kind)).collect(),
            tiers: ids.zip(trusted.iter().map(|(_, p)| u16::from(*p))).collect(),
            next_peer: 0,
            next_ask: 0,
            asks: BTreeMap::new(),
            ends: BTreeMap::new(),
            pending: Vec::new(),
            flying: BTreeMap::new(),
            benched: BTreeMap::new(),
            failures: BTreeMap::new(),
            synced: BTreeMap::new(),
            polls: BTreeMap::new(),
            answered_lies: BTreeSet::new(),
            lies: Vec::new(),
            value_from: None,
        };
        world.step(Input::Tick);
        world
    }

    fn members(&self) -> Vec<MemberId> {
        self.kinds.keys().copied().collect()
    }

    fn trusted(&self, pick: u8) -> MemberId {
        let trusted: Vec<MemberId> =
            self.members().into_iter().filter(|m| matches!(m, MemberId::Trusted(_))).collect();
        trusted[usize::from(pick) % trusted.len()]
    }

    fn play(&mut self, one: &Move, on: &dyn Fn(Kind) -> Kind) {
        match *one {
            Move::Ask { class, pick } => {
                let class = ASKED[usize::from(class) % ASKED.len()];
                let route = match class {
                    Class::Submit => Route::Only(self.trusted(pick)),
                    Class::Headers if pick % 2 == 0 => Route::Only(self.trusted(pick / 2)),
                    Class::Headers => Route::Peers,
                    Class::Bytes => {
                        let members = self.members().into_iter().enumerate();
                        let preferred = members.filter(|(i, _)| pick & (1 << (i % 8)) != 0);
                        Route::Prefer(preferred.map(|(_, m)| m).collect())
                    }
                    _ => Route::Any,
                };
                self.ask(class, route);
            }
            Move::Abandon(pick) => {
                let open: Vec<AskId> = self.asks.keys().copied().collect();
                if let Some(ask) = open.get(usize::from(pick) % open.len().max(1)).copied() {
                    self.forget(ask);
                    self.step(Input::Abandon(ask));
                }
            }
            Move::Advance(ms) => self.advance(self.now + Duration::from_millis(u64::from(ms))),
            Move::Join(kind) => {
                let peer = MemberId::Peer(PeerId(self.next_peer));
                self.next_peer += 1;
                self.kinds.insert(peer, on(kind));
                self.tiers.insert(peer, u16::from(u8::MAX) + 1);
                self.synced.insert(peer, Synced::Live);
                let MemberId::Peer(id) = peer else { unreachable!("a peer") };
                self.step(Input::Joined(id));
            }
            Move::Leave(pick) => {
                let peers: Vec<PeerId> = self
                    .members()
                    .into_iter()
                    .filter_map(|m| match m {
                        MemberId::Peer(id) => Some(id),
                        MemberId::Trusted(_) => None,
                    })
                    .collect();
                if let Some(peer) = peers.get(usize::from(pick) % peers.len().max(1)).copied() {
                    self.kinds.remove(&MemberId::Peer(peer));
                    self.step(Input::Left(peer));
                }
            }
            Move::Push { member, push } => {
                let MemberId::Trusted(member) = self.trusted(member) else {
                    unreachable!("trusted")
                };
                self.step(Input::Push { member, push });
            }
        }
    }

    fn ask(&mut self, class: Class, route: Route) -> AskId {
        let ask = AskId(self.next_ask);
        self.next_ask += 1;
        let (round, flying) = (BTreeSet::new(), BTreeMap::new());
        let asked = Asked { class, route: route.clone(), round, flying, retry_at: None };
        self.asks.insert(ask, asked);
        self.step(Input::Ask { ask, class, route });
        ask
    }

    /// Earliest due reply (one at a time: each may end asks) or core wake, until `until`
    fn advance(&mut self, until: Instant) {
        loop {
            let reply = (0..self.pending.len()).min_by_key(|i| self.pending[*i].0);
            let reply_at = reply.map(|i| self.pending[i].0);
            let next = reply_at.into_iter().chain(self.core.wake()).min().filter(|at| *at <= until);
            let Some(next) = next else { break };
            self.now = next;
            match reply.filter(|_| reply_at == Some(next)) {
                Some(due) => {
                    let (_, input) = self.pending.remove(due);
                    self.deliver(input);
                }
                None => {
                    self.step(Input::Tick);
                    let wake = self.core.wake();
                    assert!(
                        wake.is_none_or(|at| at > next),
                        "a wake is in the future after a step"
                    );
                }
            }
        }
        self.now = until;
        self.step(Input::Tick);
    }

    /// The model's own record first (as the driver frees before stepping), then the step
    fn deliver(&mut self, input: Input) {
        match &input {
            Input::Reply { ticket, reply } => {
                self.fly(ticket.member, ticket.class, false);
                let asked = self.asks.get_mut(&ticket.ask).expect("a reply of an open ask");
                asked.flying.remove(&ticket.member);
                self.count(ticket.member, *reply != Reply::NonDomain);
                if *reply == Reply::Value {
                    self.value_from = Some(*ticket);
                }
            }
            Input::Polled { member, read } => {
                let member_id = MemberId::Trusted(*member);
                self.fly(member_id, Class::Poll, false);
                self.count(member_id, read.is_some());
                if let Some(read) = read {
                    self.synced.insert(member_id, *read);
                }
            }
            _ => unreachable!("only replies and polls pend"),
        }
        self.step(input);
    }

    fn count(&mut self, member: MemberId, answered: bool) {
        let failures = self.failures.entry(member).or_default();
        *failures = if answered { 0 } else { *failures + 1 };
    }

    fn fly(&mut self, member: MemberId, class: Class, up: bool) {
        let flying = &mut self.flying.entry(member).or_insert([0; 3])[lane(class)];
        *flying = if up { *flying + 1 } else { *flying - 1 };
    }

    /// Ask gone: its pending replies dropped with its sends
    fn forget(&mut self, ask: AskId) {
        self.asks.remove(&ask);
        let mut dropped = Vec::new();
        self.pending.retain(|(_, input)| match input {
            Input::Reply { ticket, .. } if ticket.ask == ask => {
                dropped.push(*ticket);
                false
            }
            _ => true,
        });
        for ticket in dropped {
            self.fly(ticket.member, ticket.class, false);
        }
    }

    /// Lies reported once the step's outputs are handled (a caller checks after delivery)
    fn step(&mut self, input: Input) {
        let now = self.now;
        for asked in self.asks.values_mut() {
            asked.retry_at = asked.retry_at.filter(|at| *at > now);
        }
        for output in self.core.step(input, now) {
            self.handle(output);
        }
        self.rounds_over();
        self.conserving();
        for (ticket, asked) in std::mem::take(&mut self.lies) {
            self.report(ticket);
            if asked.class.retries_rounds() {
                self.ask(asked.class, asked.route);
            }
        }
    }

    fn handle(&mut self, output: Output) {
        let now = self.now;
        match output {
            Output::Send(ticket) => {
                self.oracle(ticket);
                self.fly(ticket.member, ticket.class, true);
                let asked = self.asks.get_mut(&ticket.ask).expect("oracle: an open ask");
                asked.round.insert(ticket.member);
                asked.flying.insert(ticket.member, now);
                let kind = self.kinds[&ticket.member];
                let (after, reply) = match kind {
                    Kind::Honest | Kind::Lying => (FAST, Reply::Value),
                    Kind::Slow => (SLOW, Reply::Value),
                    Kind::Lagging => (FAST, Reply::Domain),
                    Kind::Flapping if self.up() => (FAST, Reply::Value),
                    Kind::Flapping | Kind::Down => (FAST, Reply::NonDomain),
                    Kind::Silent => (TIMEOUT, Reply::NonDomain),
                };
                self.pending.push((now + after, Input::Reply { ticket, reply }));
            }
            Output::Cancel(ticket) => {
                let at = self.pending.iter().position(|(_, input)| {
                    matches!(input, Input::Reply { ticket: pending, .. } if *pending == ticket)
                });
                self.pending.remove(at.expect("a cancel for a pending send"));
                self.fly(ticket.member, ticket.class, false);
                let asked = self.asks.get_mut(&ticket.ask).expect("a cancel of an open ask");
                asked.flying.remove(&ticket.member);
            }
            Output::Answered(ticket) => {
                assert_eq!(
                    self.value_from.take(),
                    Some(ticket),
                    "T4: the value just replied, once"
                );
                assert!(self.asks.contains_key(&ticket.ask), "T4: an ask ends once");
                let asked = self.asks[&ticket.ask].clone();
                self.forget(ticket.ask);
                self.ends.insert(ticket.ask, (now, Some(ticket.member)));
                if self.kinds.get(&ticket.member) == Some(&Kind::Lying) {
                    self.answered_lies.insert(ticket.member);
                    self.lies.push((ticket, asked));
                }
            }
            Output::Unanswered(ask) => {
                let asked = self.asks.get(&ask).expect("T4: an ask ends once").clone();
                assert!(
                    !self.honest_may_serve(&asked),
                    "T9: unanswered while an honest member may serve"
                );
                self.forget(ask);
                self.ends.insert(ask, (now, None));
            }
            Output::Poll(PollOrder { member, .. }) => {
                self.polls.entry(member).or_default().push(now);
                let member_id = MemberId::Trusted(member);
                self.fly(member_id, Class::Poll, true);
                let (after, read) = match self.kinds[&member_id] {
                    Kind::Honest | Kind::Lying => (FAST, Some(Synced::Live)),
                    Kind::Slow => (SLOW, Some(Synced::Live)),
                    Kind::Lagging if self.up() => (FAST, Some(Synced::CatchingUp)),
                    Kind::Lagging => (FAST, None),
                    Kind::Flapping if self.up() => (FAST, Some(Synced::Live)),
                    Kind::Flapping | Kind::Down => (FAST, None),
                    Kind::Silent => (TIMEOUT, None),
                };
                self.pending.push((now + after, Input::Polled { member, read }));
            }
        }
    }

    fn up(&self) -> bool {
        ((self.now - self.t0).as_secs() / 20).is_multiple_of(2)
    }

    /// T8: `ticket.member` benched once more, nobody else
    fn report(&mut self, ticket: Ticket) {
        let times = |core: &TrafficCore| -> BTreeMap<MemberId, u32> {
            core.members.iter().map(|(id, m)| (*id, m.bench.map_or(0, |b| b.times))).collect()
        };
        let mut expected = times(&self.core);
        *expected.get_mut(&ticket.member).expect("a present liar") += 1;
        let count = self.benched.get(&ticket.member).map_or(0, |(_, count)| *count);
        let span = Duration::from_secs(60 << count.min(6)).min(Duration::from_secs(3600));
        self.benched.insert(ticket.member, (self.now + span, count + 1));
        self.step(Input::Report(ticket));
        assert_eq!(times(&self.core), expected, "T8: a misanswer charged to its sender alone");
    }

    /// Naive eligibility, `round` = members asked this round
    fn eligible(&self, asked: &Asked, member: MemberId) -> bool {
        let kind = matches!(member, MemberId::Trusted(_)) || asked.class.peers();
        let benched = self.benched.get(&member).is_some_and(|(until, _)| *until > self.now);
        let down = self.failures.get(&member).is_some_and(|f| *f >= DOWN_AFTER);
        let lagging =
            asked.class.needs_synced() && self.synced.get(&member) == Some(&Synced::CatchingUp);
        let untried = !asked.round.contains(&member);
        kind && asked.route.admits(member) && !benched && !down && !lagging && untried
    }

    /// First in flight of a lane = its reserve, the rest from the shared connections
    fn room(&self, member: MemberId, lane: usize) -> bool {
        let flying = self.flying.get(&member).copied().unwrap_or([0; 3]);
        match member {
            MemberId::Trusted(_) => {
                flying[lane] == 0 || flying.iter().map(|n| (*n).max(1)).sum::<u32>() < CONNECTIONS
            }
            MemberId::Peer(_) => flying.iter().sum::<u32>() == 0,
        }
    }

    /// Round not waiting, and nothing in flight or every send past its hedge floor
    fn ready(&self, asked: &Asked) -> bool {
        let latest = asked.flying.values().max();
        match (latest, asked.class.hedge_floor()) {
            _ if asked.retry_at.is_some() => false,
            (None, _) => true,
            (Some(at), Some(floor)) => *at + floor <= self.now,
            (Some(_), None) => false,
        }
    }

    /// T2, T3, synced, T5 (eligible); T6 pick; T7 a shared permit never jumps a higher lane
    fn oracle(&self, ticket: Ticket) {
        let asked = self.asks.get(&ticket.ask).expect("T4: a send for an open ask");
        let member = ticket.member;
        assert!(self.eligible(asked, member), "oracle: sent to an ineligible member ({ticket:?})");
        let own = lane(asked.class);
        let roomy: Vec<MemberId> = self
            .members()
            .into_iter()
            .filter(|m| self.eligible(asked, *m) && self.room(*m, own))
            .collect();
        let top = roomy.iter().map(|m| self.tiers[m]).min();
        let best: Vec<MemberId> =
            roomy.into_iter().filter(|m| Some(self.tiers[m]) == top).collect();
        let preferred = match &asked.route {
            Route::Prefer(prefer) => prefer.iter().find(|m| best.contains(m)).copied(),
            _ => None,
        };
        let load = |m: &MemberId| self.flying.get(m).map_or(0, |f| f.iter().sum::<u32>());
        let least = best.iter().min_by_key(|m| load(m)).copied();
        assert_eq!(
            Some(member),
            preferred.or(least),
            "T6: pick = best tier with room, preferred, least in flight ({ticket:?})"
        );
        let reserved = matches!(member, MemberId::Trusted(_))
            && self.flying.get(&member).is_none_or(|f| f[own] == 0);
        if reserved {
            return;
        }
        let poll_waits = self.core.members[&member].poller.as_ref().is_some_and(|poller| {
            !poller.in_flight
                && poller.due(self.core.members[&member].failures, self.now) <= self.now
        });
        let higher = self.asks.iter().find(|(_, other)| {
            lane(other.class) < own && self.ready(other) && self.eligible(other, member)
        });
        assert!(
            !poll_waits && higher.is_none(),
            "T7: {ticket:?} took a shared permit ahead of a due poll ({poll_waits}) or {higher:?}"
        );
    }

    /// Block ask with nothing in flight and nobody eligible left: next round after 1 s
    fn rounds_over(&mut self) {
        let over: Vec<AskId> = self
            .asks
            .iter()
            .filter(|(_, asked)| {
                asked.class.retries_rounds()
                    && asked.retry_at.is_none()
                    && asked.flying.is_empty()
                    && !self.members().into_iter().any(|m| self.eligible(asked, m))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in over {
            let asked = self.asks.get_mut(&id).expect("open");
            asked.round.clear();
            asked.retry_at = Some(self.now + ROUND_RETRY);
        }
    }

    /// T7: no ready ask waits while an eligible member has room for its lane
    fn conserving(&self) {
        for (id, asked) in &self.asks {
            let room = self.members().into_iter().find(|m| {
                self.ready(asked) && self.eligible(asked, *m) && self.room(*m, lane(asked.class))
            });
            assert_eq!(room, None, "T7: {id:?} {asked:?} waits beside a member with room");
        }
    }

    fn honest_may_serve(&self, asked: &Asked) -> bool {
        self.kinds.iter().any(|(member, kind)| {
            let kind_ok = matches!(member, MemberId::Trusted(_)) || asked.class.peers();
            matches!(kind, Kind::Honest | Kind::Slow) && kind_ok && asked.route.admits(*member)
        })
    }

    fn settle(&mut self) {
        let quiet = self.now;
        self.advance(quiet + SETTLE);
        for asked in self.asks.values() {
            assert!(
                !self.honest_may_serve(asked),
                "T9: an ask an honest member may serve still open: {asked:?}"
            );
        }
        for liar in &self.answered_lies {
            if let Some(member) = self.core.members.get(liar) {
                assert!(member.bench.is_some(), "every liar that answered benched");
            }
        }
        for (member, starts) in &self.polls {
            for gap in starts.windows(2).map(|w| w[1] - w[0]) {
                assert!(gap >= MIN_POLL_SPACING, "T10: {member:?} polled {gap:?} apart");
            }
            let settled = starts.iter().filter(|at| **at >= quiet);
            let gaps: Vec<Duration> =
                settled.collect::<Vec<_>>().windows(2).map(|w| *w[1] - *w[0]).collect();
            let bound = LADDER_CEILING + TIMEOUT;
            assert!(gaps.iter().all(|gap| *gap <= bound), "T10: {member:?} gaps {gaps:?}");
        }
    }
}
