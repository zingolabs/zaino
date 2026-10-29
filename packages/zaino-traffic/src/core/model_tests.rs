//! [`TrafficCore`] against simulated members and a naive oracle (`usage.md`)
//!
//! - Trusted 1..=4 (priorities 0..=2, 5 connections) + maybe the peer set, one [`Kind`] each
//! - Kinds: slow (past every hedge floor), lying (a wrong value), lagging (absent, polls catching
//!   up), flapping (fails in odd 20 s phases, lagging's polls too), silent (transport timeout)
//! - Swarm: whole kinds off per case (an off one plays honest)
//! - Model = the caller: a lie answered → `report` (+ a block re-asked)
//! - Peer set never benched
//! - Own lanes, permits (poll's own outside them) and rounds: the oracle's ground truth
//! - Per send: [`Ineligible`] (T2 kind + route, T3 bench + down, synced, T5), T6 pick, T7 priority
//! - After every step: `check()`, T7 no ready ask waits beside room
//! - At quiescence (10 min): T9, liars benched, T10 poll gaps ≤ the ladder's ceiling

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::{AskId, Input, Output, PollOrder, Push, Reply, Route, Ticket, TrafficCore};
use crate::class::Class;
use crate::member::{
    Limits, MemberId, Synced, TrustedValidatorId, DOWN_AFTER, LADDER_CEILING, MIN_POLL_SPACING,
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
/// Poll's 1 + lanes' 4
const CONNECTIONS: u32 = 5;
const PEER_IN_FLIGHT: u32 = 4;
const PEER_TIER: u16 = 256;
const ASKED: [Class; 6] =
    [Class::Submit, Class::TipBlock, Class::Headers, Class::Lookup, Class::Bytes, Class::BulkBlock];
/// Abandon targets: the first asks of a case (a closed or unmade one = no move)
const ABANDONABLE: u64 = 16;

/// Control, interactive, bulk (dispatch order)
fn lane(class: Class) -> usize {
    match class {
        Class::Submit | Class::Headers => 0,
        Class::TipBlock | Class::Lookup | Class::Bytes => 1,
        Class::BulkBlock => 2,
    }
}

fn trusted(index: usize) -> MemberId {
    MemberId::TrustedValidator(TrustedValidatorId::new(index).expect("≤ 4"))
}

#[derive(Debug, Clone, Copy)]
struct Trusted {
    kind: Kind,
    priority: u8,
}

#[derive(Debug, Clone)]
enum Move {
    Ask { class: Class, route: Route },
    Abandon(AskId),
    Advance(Duration),
    Push { validator: TrustedValidatorId, push: Push },
}

#[derive(Debug, Clone)]
struct Case {
    trusted: Vec<Trusted>,
    peers: Option<Kind>,
    moves: Vec<Move>,
}

fn kind() -> impl Strategy<Value = Kind> {
    prop::sample::select(KINDS.to_vec())
}

/// Every member a route may name exists in the case
fn route(class: Class, trusted_count: usize, peers: bool) -> BoxedStrategy<Route> {
    let one = (0..trusted_count).prop_map(trusted);
    match class {
        Class::Submit => one.prop_map(Route::Only).boxed(),
        Class::Headers => prop_oneof![one.prop_map(Route::Only), Just(Route::Peers)].boxed(),
        Class::Bytes => {
            let mut members: Vec<MemberId> = (0..trusted_count).map(trusted).collect();
            members.extend(peers.then_some(MemberId::Peers));
            let all = members.len();
            prop::sample::subsequence(members, 0..=all).prop_map(Route::Prefer).boxed()
        }
        _ => Just(Route::Any).boxed(),
    }
}

fn moves(trusted_count: usize, peers: bool) -> impl Strategy<Value = Vec<Move>> {
    let ask = prop::sample::select(ASKED.to_vec()).prop_flat_map(move |class| {
        route(class, trusted_count, peers).prop_map(move |route| Move::Ask { class, route })
    });
    let push = prop_oneof![Just(Push::Changed), any::<bool>().prop_map(Push::Link)];
    let push = (0..trusted_count, push).prop_map(|(index, push)| {
        let validator = TrustedValidatorId::new(index).expect("≤ 4");
        Move::Push { validator, push }
    });
    let one = prop_oneof![
        8 => ask,
        2 => (0..ABANDONABLE).prop_map(|ask| Move::Abandon(AskId(ask))),
        8 => (0u64..=20_000).prop_map(|ms| Move::Advance(Duration::from_millis(ms))),
        1 => push,
    ];
    prop::collection::vec(one, 1..64)
}

/// Swarm applied here: the shrunk case shows each member's kind as played
fn case() -> impl Strategy<Value = Case> {
    let trusted = prop::collection::vec((kind(), 0u8..=2), 1..=4);
    // never lying: its transport drops what fails a check (`PeerTransport`)
    let peers = prop::option::of(kind().prop_filter("checked", |kind| *kind != Kind::Lying));
    let swarm = prop::array::uniform7(prop::bool::weighted(0.8));
    (trusted, peers, swarm).prop_flat_map(|(trusted, peers, swarm)| {
        let on = |kind: Kind| if swarm[kind as usize] { kind } else { Kind::Honest };
        let trusted: Vec<Trusted> = trusted
            .into_iter()
            .map(|(kind, priority)| Trusted { kind: on(kind), priority })
            .collect();
        let peers = peers.map(on);
        moves(trusted.len(), peers.is_some()).prop_map(move |moves| Case {
            trusted: trusted.clone(),
            peers,
            moves,
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn every_ask_an_honest_member_may_serve_is_served_and_no_send_breaks_the_rules(
        case in case(),
    ) {
        let configured: Vec<(Kind, u8)> =
            case.trusted.iter().map(|trusted| (trusted.kind, trusted.priority)).collect();
        let mut world = World::new(&configured, case.peers)?;
        for one in &case.moves {
            world.play(one)?;
        }
        world.settle()?;
    }
}

/// Tier 0 stalls, fails or lacks it, tier 1 answers in `FAST`: each ask ends at its bound
#[test]
fn an_ask_ends_within_its_hedge_floor_plus_one_round() -> Result<(), TestCaseError> {
    use Class::{BulkBlock, Bytes, Headers, Lookup, Submit, TipBlock};
    use Kind::{Down, Lagging, Silent, Slow};
    // tier 1's answer `FAST` after the hedge floor
    let hedged = |class: Class| class.hedge_floor().expect("a hedged class") + FAST;
    let (v0, v1) = (trusted(0), trusted(1));
    let (any, pinned, prefer_none) = (Route::Any, Route::Only(v0), Route::Prefer(vec![]));
    #[rustfmt::skip]
    let cases = [
        // case,            tier 0,  class,     route,         ends after,         served by
        ("past a silence",  Silent,  TipBlock,  any.clone(),   hedged(TipBlock),   Some(v1)),
        ("past a stall",    Slow,    TipBlock,  any.clone(),   hedged(TipBlock),   Some(v1)),
        ("past a silence",  Silent,  Lookup,    any.clone(),   hedged(Lookup),     Some(v1)),
        ("past a silence",  Silent,  BulkBlock, any.clone(),   hedged(BulkBlock),  Some(v1)),
        ("absent: next",    Lagging, Lookup,    any,           FAST + FAST,        Some(v1)),
        ("failed: next",    Down,    Bytes,     prefer_none,   FAST + FAST,        Some(v1)),
        ("pinned: timeout", Silent,  Headers,   pinned.clone(), TIMEOUT,           None),
        ("pinned: failure", Down,    Submit,    pinned,        FAST,               None),
    ];
    for (case, kind, class, route, after, served_by) in cases {
        let mut world = World::new(&[(kind, 0), (Kind::Honest, 1)], None)?;
        let asked_at = world.now;
        let ask = world.ask(class, route.clone())?;
        world.advance(asked_at + Duration::from_secs(60))?;
        let ended = world.ends.get(&ask).map(|(at, by)| Ended { after: *at - asked_at, by: *by });
        let expected = Ended { after, by: served_by };
        assert_eq!(ended, Some(expected), "{case}: {kind:?} tier 0, {class:?} via {route:?}");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Ended {
    after: Duration,
    by: Option<MemberId>,
}

/// Every lane permit held by silent asks (headers + submit on control): T10, each due poll still
/// sent at once (the failed poll's answer at `TIMEOUT`, the next started in that same instant)
#[test]
fn a_due_poll_never_waits_behind_other_asks() -> Result<(), TestCaseError> {
    let v0 = trusted(0);
    let mut world = World::new(&[(Kind::Silent, 0)], None)?;
    let t0 = world.now;
    for (class, route) in [
        (Class::Headers, Route::Only(v0)),
        (Class::Submit, Route::Only(v0)),
        (Class::TipBlock, Route::Any),
        (Class::BulkBlock, Route::Any),
    ] {
        world.ask(class, route)?;
    }
    let held = world.flying[&v0];
    assert_eq!(held, [2, 1, 1], "v0's lane sends (control, interactive, bulk): all 4 permits");

    world.advance(t0 + TIMEOUT)?;

    let polls = &world.polls[&TrustedValidatorId::new(0).expect("small")];
    let since_t0: Vec<Duration> = polls.iter().map(|at| *at - t0).collect();
    assert_eq!(since_t0[..2], [Duration::ZERO, TIMEOUT], "v0's poll starts: the due one at once");
    Ok(())
}

/// First rule keeping the oracle from a member: T2 = kind / route, T3 = benched / down, T5 = tried
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ineligible {
    PeersNeverServeIt,
    OutsideItsRoute,
    Benched { for_another: Duration },
    Down { failures: u32 },
    CatchingUp,
    TriedThisRound,
}

struct World {
    core: TrafficCore,
    t0: Instant,
    now: Instant,
    kinds: BTreeMap<MemberId, Kind>,
    tiers: BTreeMap<MemberId, u16>,
    next_ask: u64,
    asks: BTreeMap<AskId, Asked>,
    ends: BTreeMap<AskId, (Instant, Option<MemberId>)>,
    pending: Vec<(Instant, Input)>,
    flying: BTreeMap<MemberId, [u32; 3]>,
    polling: BTreeSet<MemberId>,
    benched: BTreeMap<MemberId, (Instant, u32)>,
    failures: BTreeMap<MemberId, u32>,
    synced: BTreeMap<MemberId, Synced>,
    polls: BTreeMap<TrustedValidatorId, Vec<Instant>>,
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
    /// `trusted[i]` = (kind, priority) of `TrustedValidatorId(i)`
    fn new(trusted: &[(Kind, u8)], peers: Option<Kind>) -> Result<Self, TestCaseError> {
        let limits = Limits::new(CONNECTIONS).expect("MIN_CONNECTIONS");
        let config: Vec<(u8, Limits)> = trusted.iter().map(|(_, p)| (*p, limits)).collect();
        let t0 = Instant::now();
        let ids = (0..trusted.len()).map(self::trusted);
        let mut kinds: BTreeMap<MemberId, Kind> =
            ids.clone().zip(trusted.iter().map(|(kind, _)| *kind)).collect();
        let mut tiers: BTreeMap<MemberId, u16> =
            ids.zip(trusted.iter().map(|(_, p)| u16::from(*p))).collect();
        let mut synced = BTreeMap::new();
        if let Some(kind) = peers {
            kinds.insert(MemberId::Peers, kind);
            tiers.insert(MemberId::Peers, PEER_TIER);
            synced.insert(MemberId::Peers, Synced::Live);
        }
        let mut world = Self {
            core: TrafficCore::new(&config, peers.map(|_| PEER_IN_FLIGHT), t0),
            t0,
            now: t0,
            kinds,
            tiers,
            next_ask: 0,
            asks: BTreeMap::new(),
            ends: BTreeMap::new(),
            pending: Vec::new(),
            flying: BTreeMap::new(),
            polling: BTreeSet::new(),
            benched: BTreeMap::new(),
            failures: BTreeMap::new(),
            synced,
            polls: BTreeMap::new(),
            answered_lies: BTreeSet::new(),
            lies: Vec::new(),
            value_from: None,
        };
        world.step(Input::Tick)?;
        Ok(world)
    }

    fn members(&self) -> Vec<MemberId> {
        self.kinds.keys().copied().collect()
    }

    fn play(&mut self, one: &Move) -> Result<(), TestCaseError> {
        match one {
            Move::Ask { class, route } => {
                self.ask(*class, route.clone())?;
            }
            Move::Abandon(ask) if self.asks.contains_key(ask) => {
                self.forget(*ask);
                self.step(Input::Abandon(*ask))?;
            }
            Move::Abandon(_) => {}
            Move::Advance(by) => self.advance(self.now + *by)?,
            Move::Push { validator, push } => {
                self.step(Input::Push { validator: *validator, push: *push })?;
            }
        }
        Ok(())
    }

    fn ask(&mut self, class: Class, route: Route) -> Result<AskId, TestCaseError> {
        let ask = AskId(self.next_ask);
        self.next_ask += 1;
        let (round, flying) = (BTreeSet::new(), BTreeMap::new());
        let asked = Asked { class, route: route.clone(), round, flying, retry_at: None };
        self.asks.insert(ask, asked);
        self.step(Input::Ask { ask, class, route })?;
        Ok(ask)
    }

    /// Earliest due reply (one at a time: each may end asks) or core wake, until `until`
    fn advance(&mut self, until: Instant) -> Result<(), TestCaseError> {
        loop {
            let reply = (0..self.pending.len()).min_by_key(|i| self.pending[*i].0);
            let reply_at = reply.map(|i| self.pending[i].0);
            let next = reply_at.into_iter().chain(self.core.wake()).min().filter(|at| *at <= until);
            let Some(next) = next else { break };
            self.now = next;
            match reply.filter(|_| reply_at == Some(next)) {
                Some(due) => {
                    let (_, input) = self.pending.remove(due);
                    self.deliver(input)?;
                }
                None => {
                    self.step(Input::Tick)?;
                    let wake = self.core.wake().map(|at| at - self.t0);
                    let stepped = next - self.t0;
                    prop_assert!(
                        wake.is_none_or(|at| at > stepped),
                        "a wake lies in the future after a step: woken at {stepped:?}, next wake \
                         {wake:?} (since t0)"
                    );
                }
            }
        }
        self.now = until;
        self.step(Input::Tick)
    }

    /// The model's own record first (as the driver frees before stepping), then the step
    fn deliver(&mut self, input: Input) -> Result<(), TestCaseError> {
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
            Input::Polled { validator, read } => {
                let member_id = MemberId::TrustedValidator(*validator);
                self.polling.remove(&member_id);
                self.count(member_id, read.is_some());
                if let Some(read) = read {
                    self.synced.insert(member_id, *read);
                }
            }
            _ => unreachable!("only replies and polls pend"),
        }
        self.step(input)
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
    fn step(&mut self, input: Input) -> Result<(), TestCaseError> {
        let now = self.now;
        for asked in self.asks.values_mut() {
            asked.retry_at = asked.retry_at.filter(|at| *at > now);
        }
        for output in self.core.step(input, now) {
            self.handle(output)?;
        }
        self.core.check();
        self.rounds_over();
        self.conserving()?;
        for (ticket, asked) in std::mem::take(&mut self.lies) {
            self.report(ticket)?;
            if asked.class.retries_rounds() {
                self.ask(asked.class, asked.route)?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, output: Output) -> Result<(), TestCaseError> {
        let now = self.now;
        match output {
            Output::Send(ticket) => {
                self.oracle(ticket)?;
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
            Output::Answered(ticket) => {
                let replied = self.value_from.take();
                prop_assert_eq!(
                    replied,
                    Some(ticket),
                    "an answer delivers the value just replied, once (T4): replied vs answered"
                );
                let asked = self.asks.get(&ticket.ask).cloned();
                let Some(asked) = asked else {
                    return Err(TestCaseError::fail(format!(
                        "an ask ends once (T4): {ticket:?} answered a closed ask"
                    )));
                };
                self.forget(ticket.ask);
                self.ends.insert(ticket.ask, (now, Some(ticket.member)));
                if self.kinds.get(&ticket.member) == Some(&Kind::Lying) {
                    self.answered_lies.insert(ticket.member);
                    self.lies.push((ticket, asked));
                }
            }
            Output::Unanswered(ask) => {
                let Some(asked) = self.asks.get(&ask).cloned() else {
                    return Err(TestCaseError::fail(format!(
                        "an ask ends once (T4): {ask:?} unanswered after it ended"
                    )));
                };
                let honest = self.honest_may_serve(&asked);
                prop_assert!(
                    honest.is_none(),
                    "an ask an honest member may serve is answered (T9): {ask:?} {asked:?} \
                     unanswered, {honest:?} honest"
                );
                self.forget(ask);
                self.ends.insert(ask, (now, None));
            }
            Output::Poll(PollOrder { validator, .. }) => {
                self.polls.entry(validator).or_default().push(now);
                let member_id = MemberId::TrustedValidator(validator);
                prop_assert!(
                    self.polling.insert(member_id),
                    "one poll per member in flight (T10): {member_id:?} polled again mid-poll"
                );
                let (after, read) = match self.kinds[&member_id] {
                    Kind::Honest | Kind::Lying => (FAST, Some(Synced::Live)),
                    Kind::Slow => (SLOW, Some(Synced::Live)),
                    Kind::Lagging if self.up() => (FAST, Some(Synced::CatchingUp)),
                    Kind::Lagging => (FAST, None),
                    Kind::Flapping if self.up() => (FAST, Some(Synced::Live)),
                    Kind::Flapping | Kind::Down => (FAST, None),
                    Kind::Silent => (TIMEOUT, None),
                };
                self.pending.push((now + after, Input::Polled { validator, read }));
            }
        }
        Ok(())
    }

    fn up(&self) -> bool {
        ((self.now - self.t0).as_secs() / 20).is_multiple_of(2)
    }

    /// T8: a trusted `ticket.member` benched once more, nobody else (the peer set: nobody)
    fn report(&mut self, ticket: Ticket) -> Result<(), TestCaseError> {
        let times = |core: &TrafficCore| -> BTreeMap<MemberId, u32> {
            core.members.iter().map(|(id, m)| (*id, m.bench.map_or(0, |b| b.times))).collect()
        };
        let mut expected = times(&self.core);
        if let MemberId::TrustedValidator(_) = ticket.member {
            *expected.get_mut(&ticket.member).expect("a present liar") += 1;
            let count = self.benched.get(&ticket.member).map_or(0, |(_, count)| *count);
            let span = Duration::from_secs(60 << count.min(6)).min(Duration::from_secs(3600));
            self.benched.insert(ticket.member, (self.now + span, count + 1));
        }
        self.step(Input::Report(ticket))?;
        prop_assert_eq!(
            times(&self.core),
            expected,
            "a misanswer charged to its sender alone (T8): times benched per member after \
             reporting {:?}",
            ticket
        );
        Ok(())
    }

    /// Naive eligibility, `round` = members asked this round; the first rule `member` fails
    fn ineligible(&self, asked: &Asked, member: MemberId) -> Option<Ineligible> {
        let is_trusted = matches!(member, MemberId::TrustedValidator(_));
        if !is_trusted && !asked.class.peers() {
            return Some(Ineligible::PeersNeverServeIt);
        }
        if !asked.route.admits(member) {
            return Some(Ineligible::OutsideItsRoute);
        }
        if let Some((until, _)) = self.benched.get(&member).filter(|(until, _)| *until > self.now) {
            return Some(Ineligible::Benched { for_another: *until - self.now });
        }
        // down needs a poll probe back up: the peer set never goes down
        let failures = self.failures.get(&member).copied().unwrap_or(0);
        if is_trusted && failures >= DOWN_AFTER {
            return Some(Ineligible::Down { failures });
        }
        if asked.class.needs_synced() && self.synced.get(&member) == Some(&Synced::CatchingUp) {
            return Some(Ineligible::CatchingUp);
        }
        asked.round.contains(&member).then_some(Ineligible::TriedThisRound)
    }

    fn eligible(&self, asked: &Asked, member: MemberId) -> bool {
        self.ineligible(asked, member).is_none()
    }

    /// First in flight of a lane = its reserve, the rest from the shared connections (a trusted
    /// poll's one never counted)
    fn room(&self, member: MemberId, lane: usize) -> bool {
        let flying = self.flying.get(&member).copied().unwrap_or([0; 3]);
        let max = match member {
            MemberId::TrustedValidator(_) => CONNECTIONS - 1,
            MemberId::Peers => PEER_IN_FLIGHT,
        };
        flying[lane] == 0 || flying.iter().map(|n| (*n).max(1)).sum::<u32>() < max
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

    /// - T2, T3, synced, T5 ([`Ineligible`])
    /// - T6 pick
    /// - T7 a shared permit never jumps a higher lane
    fn oracle(&self, ticket: Ticket) -> Result<(), TestCaseError> {
        let Some(asked) = self.asks.get(&ticket.ask) else {
            return Err(TestCaseError::fail(format!("a send for an open ask (T4): {ticket:?}")));
        };
        let member = ticket.member;
        if let Some(rule) = self.ineligible(asked, member) {
            return Err(TestCaseError::fail(format!(
                "sent to an ineligible member, {rule:?}: {member:?} for {:?} {:?} via {:?}",
                ticket.ask, asked.class, asked.route
            )));
        }
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
        let load = |m: &MemberId| {
            let lanes = self.flying.get(m).map_or(0, |f| f.iter().sum::<u32>());
            lanes + u32::from(self.polling.contains(m))
        };
        let least = best.iter().min_by_key(|m| load(m)).copied();
        prop_assert_eq!(
            Some(member),
            preferred.or(least),
            "pick = best tier with room, then preferred, then least in flight (T6): sent vs the \
             oracle's pick for {:?} {:?} via {:?}, best tier {:?}",
            ticket.ask,
            asked.class,
            asked.route,
            best
        );
        let reserved = self.flying.get(&member).is_none_or(|f| f[own] == 0);
        if reserved {
            return Ok(());
        }
        let higher = self.asks.iter().find(|(_, other)| {
            lane(other.class) < own && self.ready(other) && self.eligible(other, member)
        });
        prop_assert!(
            higher.is_none(),
            "a shared permit never jumps a higher lane's waiting ask (T7): {ticket:?} ahead of \
             {higher:?}"
        );
        Ok(())
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
    fn conserving(&self) -> Result<(), TestCaseError> {
        for (id, asked) in &self.asks {
            let room = self.members().into_iter().find(|m| {
                self.ready(asked) && self.eligible(asked, *m) && self.room(*m, lane(asked.class))
            });
            prop_assert_eq!(
                room,
                None,
                "no ready ask waits beside a member with room (T7): {:?} {:?}",
                id,
                asked
            );
        }
        Ok(())
    }

    /// Some honest member (honest or slow) `asked` may reach, by kind and route
    fn honest_may_serve(&self, asked: &Asked) -> Option<MemberId> {
        self.kinds
            .iter()
            .find(|(member, kind)| {
                let kind_ok =
                    matches!(member, MemberId::TrustedValidator(_)) || asked.class.peers();
                matches!(kind, Kind::Honest | Kind::Slow) && kind_ok && asked.route.admits(**member)
            })
            .map(|(member, _)| *member)
    }

    fn settle(&mut self) -> Result<(), TestCaseError> {
        let quiet = self.now;
        self.advance(quiet + SETTLE)?;
        for (id, asked) in &self.asks {
            let honest = self.honest_may_serve(asked);
            prop_assert!(
                honest.is_none(),
                "an ask an honest member may serve is served (T9): {id:?} {asked:?} still open \
                 after {SETTLE:?} quiet, {honest:?} honest"
            );
        }
        for liar in self.answered_lies.iter().filter(|m| matches!(m, MemberId::TrustedValidator(_)))
        {
            let member = self.core.members.get(liar).expect("a configured liar");
            prop_assert!(
                member.bench.is_some(),
                "every trusted liar that answered benched: {liar:?}"
            );
        }
        let peers = self.core.members.get(&MemberId::Peers).and_then(|peers| peers.bench);
        prop_assert!(peers.is_none(), "the peer set never benched: {peers:?}");
        for (member, starts) in &self.polls {
            for gap in starts.windows(2).map(|w| w[1] - w[0]) {
                prop_assert!(
                    gap >= MIN_POLL_SPACING,
                    "polls of one member ≥ {MIN_POLL_SPACING:?} apart (T10): {member:?} polled \
                     {gap:?} apart"
                );
            }
            let settled = starts.iter().filter(|at| **at >= quiet);
            let gaps: Vec<Duration> =
                settled.collect::<Vec<_>>().windows(2).map(|w| *w[1] - *w[0]).collect();
            prop_assert!(
                gaps.iter().all(|gap| *gap <= LADDER_CEILING),
                "settled poll gaps ≤ the ladder's {LADDER_CEILING:?} ceiling (T10): {member:?} \
                 gaps {gaps:?}"
            );
        }
        Ok(())
    }
}
