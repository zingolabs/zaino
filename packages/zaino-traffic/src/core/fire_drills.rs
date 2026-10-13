//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::time::{Duration, Instant};

use super::{Ask, AskId, Input, Reply, Route, Ticket, TrafficCore};
use crate::class::{Class, Lane, Permits};
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

fn member(core: &mut TrafficCore, index: usize) -> &mut Member {
    core.members.get_mut(&trusted(index)).expect("configured")
}

fn ask(core: &mut TrafficCore, id: u64) -> &mut Ask {
    core.asks.get_mut(&AskId(id)).expect("open")
}

#[test]
fn the_unplanted_core_passes_every_check_in_its_planted_state() {
    let core = valid(Instant::now());
    core.check();
    let sent: Vec<(AskId, MemberId)> =
        core.asks.iter().flat_map(|(id, ask)| ask.sends.iter().map(|s| (*id, s.member))).collect();
    let expected = [(0, trusted(0)), (1, trusted(0)), (2, MemberId::Peers), (3, trusted(0))];
    assert_eq!(sent, expected.map(|(ask, member)| (AskId(ask), member)));
}

#[test]
#[should_panic(expected = "T1: in flight within max_connections, reserves never borrowed")]
fn check_catches_in_flight_past_max_connections() {
    let mut core = valid(Instant::now());
    member(&mut core, 0).in_flight[Lane::Interactive.index()] += 1;
    core.check();
}

#[test]
#[should_panic(expected = "T4: in flight = open asks' sends")]
fn check_catches_in_flight_off_the_open_sends() {
    let mut core = valid(Instant::now());
    member(&mut core, 0).in_flight[Lane::Interactive.index()] = 1;
    core.check();
}

#[test]
#[should_panic(expected = "a send's member")]
fn check_catches_a_send_to_a_member_gone() {
    let mut core = valid(Instant::now());
    core.members.remove(&MemberId::Peers);
    core.check();
}

#[test]
#[should_panic(expected = "T3: no send to a benched member")]
fn check_catches_a_send_to_a_benched_member() {
    let mut core = valid(Instant::now());
    let until = core.now + Duration::from_secs(60);
    member(&mut core, 0).bench = Some(Bench { until, times: 1, stamp: 0 });
    core.check();
}

#[test]
#[should_panic(expected = "T3: no send to a down member")]
fn check_catches_a_send_to_a_down_member() {
    let mut core = valid(Instant::now());
    member(&mut core, 0).down_since = Some(0);
    core.check();
}

#[test]
#[should_panic(expected = "T10: polls of one member at least 200 ms apart")]
fn check_catches_polls_closer_than_the_floor() {
    let mut core = valid(Instant::now());
    let poller = member(&mut core, 1).poller.as_mut().expect("trusted");
    poller.previous = poller.last.map(|last| last - Duration::from_millis(100));
    core.check();
}

#[test]
#[should_panic(expected = "T10: a due poll is sent")]
fn check_catches_a_due_poll_never_sent() {
    let mut core = valid(Instant::now());
    let now = core.now;
    member(&mut core, 1).poller.as_mut().expect("trusted").last =
        Some(now - Duration::from_secs(9));
    core.check();
}

#[test]
#[should_panic(expected = "T2: a class reaches only the member kinds its row allows")]
fn check_catches_a_send_outside_its_route() {
    let mut core = valid(Instant::now());
    ask(&mut core, 0).route = Route::Peers;
    core.check();
}

#[test]
#[should_panic(expected = "T5: a round asks each member once")]
fn check_catches_a_round_asking_a_member_twice() {
    let mut core = valid(Instant::now());
    ask(&mut core, 0).tried.clear();
    core.check();
}

#[test]
#[should_panic(expected = "T6: a send goes to the best tier with room")]
fn check_catches_a_send_past_the_best_tier_with_room() {
    let mut core = valid(Instant::now());
    ask(&mut core, 0).sends[0].top = 1;
    core.check();
}

#[test]
#[should_panic(expected = "a trusted validator to ask")]
fn a_core_without_trusted_validators_panics() {
    TrafficCore::new(&[], None, Instant::now());
}

#[test]
#[should_panic(expected = "at most ValidatorId::MAX trusted validators")]
fn a_core_past_the_validator_limit_panics() {
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    TrafficCore::new(&[(0, limits); 65], None, Instant::now());
}

#[test]
#[should_panic(expected = "the peer set's in flight covers every lane reserve")]
fn peer_permits_short_of_the_reserves_panic() {
    Permits::peers(2);
}

#[test]
#[should_panic(expected = "time never runs back")]
fn a_step_back_in_time_panics() {
    let t0 = Instant::now();
    valid(t0).step(Input::Tick, t0);
}

#[test]
#[should_panic(expected = "a fresh ask id")]
fn an_ask_id_reused_panics() {
    let t0 = Instant::now();
    let ask = Input::Ask { ask: AskId(0), class: Class::Lookup, route: Route::Any };
    valid(t0).step(ask, t0 + Duration::from_millis(50));
}

#[test]
#[should_panic(expected = "a reply for a send in flight")]
fn a_reply_without_a_send_panics() {
    let t0 = Instant::now();
    let ticket = Ticket { ask: AskId(0), member: trusted(1), class: Class::TipBlock };
    valid(t0).step(Input::Reply { ticket, reply: Reply::Value }, t0 + Duration::from_millis(50));
}

#[test]
#[should_panic(expected = "abandon: an open ask")]
fn abandoning_an_unknown_ask_panics() {
    let t0 = Instant::now();
    valid(t0).step(Input::Abandon(AskId(9)), t0 + Duration::from_millis(50));
}

#[test]
#[should_panic(expected = "a poll result for a poll in flight")]
fn a_poll_result_without_a_poll_panics() {
    let t0 = Instant::now();
    let member = ValidatorId::new(0).expect("small");
    valid(t0).step(Input::Polled { member, read: None }, t0 + Duration::from_millis(50));
}

#[test]
#[should_panic(expected = "a configured validator")]
fn a_push_from_an_unknown_validator_panics() {
    let t0 = Instant::now();
    let member = ValidatorId::new(5).expect("small");
    let push = Input::Push { member, push: super::Push::Changed };
    valid(t0).step(push, t0 + Duration::from_millis(50));
}
