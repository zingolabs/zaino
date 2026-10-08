//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::time::{Duration, Instant};

use super::{Ask, AskId, Input, Output, Reply, Route, Ticket, TrafficCore};
use crate::class::{Class, Lane, Permits};
use crate::fired;
use crate::member::{Bench, Limits, Member, MemberId, PeerId, Synced, ValidatorId};

fn trusted(index: usize) -> MemberId {
    MemberId::Trusted(ValidatorId::new(index).expect("small"))
}

/// - trusted 0 (priority 0), 1 (priority 1), both polled `Live`; peer 7 joined
/// - in flight: A0 tip block, A1 lookup, A3 bulk block on trusted 0 (its 4 permits); A2 headers
///   on peer 7
fn valid(t0: Instant) -> TrafficCore {
    let limits = Limits::new(Limits::MIN_CONNECTIONS, None).expect("MIN_CONNECTIONS");
    let mut core = TrafficCore::new(&[(0, limits), (1, limits)], t0);
    core.step(Input::Tick, t0);
    let t = t0 + Duration::from_millis(50);
    for member in [0, 1].map(|index| ValidatorId::new(index).expect("small")) {
        core.step(Input::Polled { member, read: Some(Synced::Live) }, t);
    }
    core.step(Input::Joined(PeerId(7)), t);
    let asks = [
        (Class::TipBlock, Route::Any),
        (Class::Lookup, Route::Any),
        (Class::Headers, Route::Peers),
        (Class::BulkBlock, Route::Any),
    ];
    for (id, (class, route)) in asks.into_iter().enumerate() {
        core.step(Input::Ask { ask: AskId(id as u64), class, route }, t);
    }
    core
}

#[test]
fn every_check_and_precondition_fires_on_its_planted_bug() {
    let t0 = Instant::now();
    let core = valid(t0);
    core.check();
    let sent: Vec<(AskId, MemberId)> =
        core.asks.iter().flat_map(|(id, ask)| ask.sends.iter().map(|s| (*id, s.member))).collect();
    let peer = MemberId::Peer(PeerId(7));
    let expected = [(0, trusted(0)), (1, trusted(0)), (2, peer), (3, trusted(0))];
    assert_eq!(sent, expected.map(|(ask, member)| (AskId(ask), member)), "the planted state");

    type Plant = Box<dyn Fn(&mut TrafficCore)>;
    fn member(core: &mut TrafficCore, index: usize) -> &mut Member {
        core.members.get_mut(&trusted(index)).expect("configured")
    }
    fn ask(core: &mut TrafficCore, id: u64) -> &mut Ask {
        core.asks.get_mut(&AskId(id)).expect("open")
    }
    let drills: Vec<(&str, Plant)> = vec![
        (
            "T1: in flight within max_connections, reserves never borrowed",
            Box::new(move |c| member(c, 0).in_flight[Lane::Interactive.index()] += 1),
        ),
        (
            "T4: in flight = open asks' sends + polls",
            Box::new(move |c| member(c, 0).in_flight[Lane::Interactive.index()] = 1),
        ),
        ("T4: no send to a departed member", Box::new(move |c| _ = c.members.remove(&peer))),
        (
            "T3: no send to a benched member",
            Box::new(move |c| {
                let until = c.now + Duration::from_secs(60);
                member(c, 0).bench = Some(Bench { until, times: 1, stamp: 0 });
            }),
        ),
        ("T3: no send to a down member", Box::new(move |c| member(c, 0).down_since = Some(0))),
        (
            "T10: polls of one member at least 200 ms apart",
            Box::new(move |c| {
                let poller = member(c, 1).poller.as_mut().expect("trusted");
                poller.previous = poller.last.map(|last| last - Duration::from_millis(100));
            }),
        ),
        (
            "T10: a due poll is sent while its control lane has room",
            Box::new(move |c| {
                let now = c.now;
                member(c, 1).poller.as_mut().expect("trusted").last =
                    Some(now - Duration::from_secs(9));
            }),
        ),
        (
            "T2: a class reaches only the member kinds its row allows",
            Box::new(move |c| ask(c, 0).route = Route::Peers),
        ),
        ("T5: a round asks each member once", Box::new(move |c| ask(c, 0).tried.clear())),
        (
            "T6: a send goes to the best tier with room",
            Box::new(move |c| ask(c, 0).sends[0].top = 1),
        ),
    ];
    for (expected, plant) in drills {
        let mut core = valid(t0);
        plant(&mut core);
        let message = fired(|| core.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: caller / driver bug → named panic
    let t = t0 + Duration::from_millis(50);
    let step = |input: Input, now: Instant| fired(|| drop(valid(t0).step(input, now)));
    let limits = Limits::new(Limits::MIN_CONNECTIONS, None).expect("MIN_CONNECTIONS");
    let ticket = Ticket { ask: AskId(0), member: trusted(1), class: Class::TipBlock };
    let v = |index| ValidatorId::new(index).expect("small");
    let preconditions = [
        ("a trusted validator to ask", fired(|| drop(TrafficCore::new(&[], t0)))),
        (
            "at most ValidatorId::MAX trusted validators",
            fired(|| drop(TrafficCore::new(&[(0, limits); 65], t0))),
        ),
        (
            "max_connections covers every lane reserve + one shared",
            fired(|| _ = Permits::trusted(3)),
        ),
        ("time never runs back", step(Input::Tick, t0)),
        (
            "a poll is the core's own, never asked",
            step(Input::Ask { ask: AskId(9), class: Class::Poll, route: Route::Any }, t),
        ),
        (
            "a fresh ask id",
            step(Input::Ask { ask: AskId(0), class: Class::Lookup, route: Route::Any }, t),
        ),
        ("a reply for a send in flight", step(Input::Reply { ticket, reply: Reply::Value }, t)),
        ("abandon: an open ask", step(Input::Abandon(AskId(9)), t)),
        ("a poll result for a poll in flight", step(Input::Polled { member: v(0), read: None }, t)),
        (
            "a configured validator",
            step(Input::Push { member: v(5), push: super::Push::Changed }, t),
        ),
        ("a peer joins once", step(Input::Joined(PeerId(7)), t)),
        ("a peer leaves once, after joining", step(Input::Left(PeerId(8)), t)),
    ];
    for (expected, message) in preconditions {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    // not a bug: a peer leaving mid-send cancels it, its ask moves on (a lookup never reached it)
    let mut core = valid(t0);
    let out = core.step(Input::Left(PeerId(7)), t);
    let cancelled = Ticket { ask: AskId(2), member: peer, class: Class::Headers };
    assert_eq!(out, [Output::Cancel(cancelled), Output::Unanswered(AskId(2))]);
}
