//! [`TrafficCore`] against simulated members and a naive oracle (`traffic-balancer.md` §8)
//!
//! - Trusted 1..=4 (priorities 0..=2) + peers joining / leaving, one [`Kind`] each
//! - Kinds: slow (past every hedge floor), lying (a wrong value), lagging (absent; polls catching
//!   up), flapping (fails in odd 20 s phases, lagging's polls too), silent (transport timeout)
//! - Swarm: whole kinds off per case (an off one plays honest)
//! - Asks of every class and route, abandons, push events, clock advances
//! - Model = the caller: a lie answered → `report` (+ a block re-asked)
//! - Oracle per send, from the model's own record: T2 kind + route, T3 bench + down, synced,
//!   T5 one member once per ask (rounds: blocks only), T6 no better tier eligible with room
//! - Quiescence (no asks, 10 min): T9 every ask an honest member may serve answered, every
//!   present liar that answered benched, T10 poll gaps ≤ the ladder's ceiling

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::{AskId, Input, Output, PollOrder, Push, Reply, Route, Ticket, TrafficCore};
use crate::class::{Class, PerClass, CLASSES};
use crate::limits::{Permits, MIN_CONNECTIONS};
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
const ASKED: [Class; 6] =
    [Class::Submit, Class::TipBlock, Class::Headers, Class::Lookup, Class::Bytes, Class::BulkBlock];

#[derive(Debug, Clone)]
enum Move {
    Ask { class: u8, pick: u8, cost: u8 },
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
        8 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(class, pick, cost)| Move::Ask { class, pick, cost }),
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

    /// - `check()` after every step (T1–T8, T10), the oracle after every send
    /// - Quiescence: T9, liars benched, T10 gaps
    #[test]
    fn every_ask_an_honest_member_may_serve_is_served_and_no_send_breaks_the_rules(
        trusted in prop::collection::vec((kind(), 0u8..=2), 1..=4),
        moves in moves(),
        swarm in prop::array::uniform7(prop::bool::weighted(0.8)),
        seed in any::<u64>(),
    ) {
        let on = |kind: Kind| if swarm[kind as usize] { kind } else { Kind::Honest };
        let trusted: Vec<(Kind, u8)> = trusted.into_iter().map(|(kind, p)| (on(kind), p)).collect();
        let mut world = World::new(&trusted, seed);
        for one in &moves {
            world.play(one, &on);
        }
        world.settle();
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
    pending: Vec<(Instant, Input)>,
    flying: BTreeMap<MemberId, PerClass<u32>>,
    benched: BTreeMap<MemberId, (Instant, u32)>,
    failures: BTreeMap<MemberId, u32>,
    synced: BTreeMap<MemberId, Synced>,
    polls: BTreeMap<ValidatorId, Vec<Instant>>,
    answered_lies: BTreeSet<MemberId>,
    lies: Vec<(Ticket, Asked)>,
    value_from: Option<Ticket>,
}

/// `sent` = every member sent to, all rounds
#[derive(Debug, Clone)]
struct Asked {
    class: Class,
    route: Route,
    sent: BTreeSet<MemberId>,
}

impl World {
    fn new(trusted: &[(Kind, u8)], seed: u64) -> Self {
        let limits = Limits::new(MIN_CONNECTIONS, None).expect("MIN_CONNECTIONS");
        let config: Vec<(u8, Limits)> = trusted.iter().map(|(_, p)| (*p, limits)).collect();
        let t0 = Instant::now();
        let ids = (0..trusted.len()).map(|i| MemberId::Trusted(ValidatorId::new(i).expect("≤ 4")));
        let mut world = Self {
            core: TrafficCore::new(&config, seed, t0),
            t0,
            now: t0,
            kinds: ids.clone().zip(trusted.iter().map(|(kind, _)| *kind)).collect(),
            tiers: ids.zip(trusted.iter().map(|(_, p)| u16::from(*p))).collect(),
            next_peer: 0,
            next_ask: 0,
            asks: BTreeMap::new(),
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
            Move::Ask { class, pick, cost } => {
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
                self.ask(class, route, 1 + u32::from(cost % 4));
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

    fn ask(&mut self, class: Class, route: Route, cost: u32) {
        let ask = AskId(self.next_ask);
        self.next_ask += 1;
        let asked = Asked { class, route: route.clone(), sent: BTreeSet::new() };
        self.asks.insert(ask, asked);
        self.step(Input::Ask { ask, class, route, cost });
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
        let flying = &mut self.flying.entry(member).or_insert([0; CLASSES])[class.index()];
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
        for output in self.core.step(input, self.now) {
            self.handle(output);
        }
        for (ticket, asked) in std::mem::take(&mut self.lies) {
            self.report(ticket);
            if asked.class.retries_rounds() {
                self.ask(asked.class, asked.route, 1);
            }
        }
    }

    fn handle(&mut self, output: Output) {
        let now = self.now;
        match output {
            Output::Send(ticket) => {
                self.oracle(ticket);
                self.fly(ticket.member, ticket.class, true);
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

    /// Naive eligibility, `tried` = every member this ask ever reached
    fn eligible(&self, asked: &Asked, member: MemberId) -> bool {
        let kind = matches!(member, MemberId::Trusted(_)) || asked.class.peers();
        let benched = self.benched.get(&member).is_some_and(|(until, _)| *until > self.now);
        let down = self.failures.get(&member).is_some_and(|f| *f >= DOWN_AFTER);
        let lagging =
            asked.class.needs_synced() && self.synced.get(&member) == Some(&Synced::CatchingUp);
        kind && asked.route.admits(member) && !benched && !down && !lagging
    }

    fn room(&self, member: MemberId, class: Class) -> bool {
        let permits = match member {
            MemberId::Trusted(_) => Permits::trusted(MIN_CONNECTIONS),
            MemberId::Peer(_) => Permits::peer(),
        };
        permits.admits(&self.flying.get(&member).copied().unwrap_or([0; CLASSES]), class)
    }

    fn oracle(&mut self, ticket: Ticket) {
        let asked = self.asks.get(&ticket.ask).expect("T4: a send for an open ask").clone();
        assert!(
            self.eligible(&asked, ticket.member),
            "oracle: sent to an ineligible member ({ticket:?})"
        );
        let again = asked.sent.contains(&ticket.member);
        assert!(!again || asked.class.retries_rounds(), "T5: a member asked twice outside rounds");
        let tier = self.tiers[&ticket.member];
        let better = self.members().into_iter().filter(|m| self.tiers[m] < tier).find(|m| {
            !asked.sent.contains(m) && self.eligible(&asked, *m) && self.room(*m, asked.class)
        });
        assert_eq!(better, None, "T6: a better tier had room for {ticket:?}");
        self.asks.get_mut(&ticket.ask).expect("open").sent.insert(ticket.member);
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
            assert!(gaps.iter().all(|gap| *gap <= LADDER_CEILING), "T10: {member:?} gaps {gaps:?}");
        }
    }
}
