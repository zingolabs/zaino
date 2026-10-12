//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::time::{Duration, Instant};

use super::{Ask, AskId, Input, Reply, Route, Ticket, TrafficCore};
use crate::class::{Class, Lane, Permits};
use crate::fired;
use crate::member::{Bench, Limits, Member, MemberId, Synced, ValidatorId};

fn trusted(index: usize) -> MemberId {
    MemberId::Trusted(ValidatorId::new(index).expect("small"))
}

/// - trusted 0 (priority 0), 1 (priority 1), both polled `Live`; the peer set (3 in flight)
/// - in flight: A0 tip block, A1 lookup, A3 bulk block on trusted 0 (its 4 lane permits); A2
///   headers on the peer set
fn valid(t0: Instant) -> TrafficCore {
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    let mut core = TrafficCore::new(&[(0, limits), (1, limits)], Some(3), t0);
    core.step(Input::Tick, t0);
    let t = t0 + Duration::from_millis(50);
    for member in [0, 1].map(|index| ValidatorId::new(index).expect("small")) {
        core.step(Input::Polled { member, read: Some(Synced::Live) }, t);
    }
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
    let peer = MemberId::Peers;
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
            "T4: in flight = open asks' sends",
            Box::new(move |c| member(c, 0).in_flight[Lane::Interactive.index()] = 1),
        ),
        ("a send's member", Box::new(move |c| _ = c.members.remove(&peer))),
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
            "T10: a due poll is sent",
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
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    let ticket = Ticket { ask: AskId(0), member: trusted(1), class: Class::TipBlock };
    let v = |index| ValidatorId::new(index).expect("small");
    let preconditions = [
        ("a trusted validator to ask", fired(|| drop(TrafficCore::new(&[], None, t0)))),
        (
            "at most ValidatorId::MAX trusted validators",
            fired(|| drop(TrafficCore::new(&[(0, limits); 65], None, t0))),
        ),
        (
            "max_connections covers the poll, every lane reserve + one shared",
            fired(|| _ = Permits::trusted(4)),
        ),
        ("the peer set's in flight covers every lane reserve", fired(|| _ = Permits::peers(2))),
        ("time never runs back", step(Input::Tick, t0)),
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
    ];
    for (expected, message) in preconditions {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    // not a bug: a report on the peer set benches nothing (its transport blames the peer)
    let mut core = valid(t0);
    let headers = Ticket { ask: AskId(2), member: peer, class: Class::Headers };
    core.step(Input::Report(headers), t);
    assert!(core.members.values().all(|member| member.bench.is_none()), "nobody benched");
}
