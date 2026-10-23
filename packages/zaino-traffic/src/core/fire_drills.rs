//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::time::{Duration, Instant};

use super::{Ask, AskId, Input, Push, Reply, Route, Sent, Ticket, TrafficCore};
use crate::class::{Class, Lane, Permits};
use crate::member::{Limits, Member, MemberId, Synced, TrustedValidatorId};

const TIP_BLOCK: AskId = AskId(0);
const LOOKUP: AskId = AskId(1);
const PEER_HEADERS: AskId = AskId(2);
const BULK_BLOCK: AskId = AskId(3);

fn trusted(index: usize) -> MemberId {
    MemberId::TrustedValidator(TrustedValidatorId::new(index).expect("small"))
}

/// Trusted 0 + 1 (priorities 0, 1, `MIN_CONNECTIONS` each) polled `Live`, peer set (3 in flight),
/// the four asks above open; where each lands = [`the_unplanted_core_passes_check`]
fn two_live_trusted_and_peers_with_four_asks_open(t0: Instant) -> TrafficCore {
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    let mut core = TrafficCore::new(&[(0, limits), (1, limits)], Some(3), t0);
    core.step(Input::Tick, t0);
    let t = t0 + Duration::from_millis(50);
    for validator in [0, 1].map(|index| TrustedValidatorId::new(index).expect("small")) {
        core.step(Input::Polled { validator, read: Some(Synced::Live) }, t);
    }
    for (ask, class, route) in [
        (TIP_BLOCK, Class::TipBlock, Route::Any),
        (LOOKUP, Class::Lookup, Route::Any),
        (PEER_HEADERS, Class::Headers, Route::Peers),
        (BULK_BLOCK, Class::BulkBlock, Route::Any),
    ] {
        core.step(Input::Ask { ask, class, route }, t);
    }
    core
}

fn member(core: &mut TrafficCore, index: usize) -> &mut Member {
    core.members.get_mut(&trusted(index)).expect("configured")
}

fn ask(core: &mut TrafficCore, id: AskId) -> &mut Ask {
    core.asks.get_mut(&id).expect("open")
}

/// Trusted 0 = priority 0 with room for 4 lane sends: all three of its asks land there
#[test]
fn the_unplanted_core_passes_check() {
    let core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());

    core.check();

    let sent: Vec<(AskId, MemberId)> =
        core.asks.iter().flat_map(|(id, ask)| ask.sends.iter().map(|s| (*id, s.member))).collect();
    let expected = [
        (TIP_BLOCK, trusted(0)),
        (LOOKUP, trusted(0)),
        (PEER_HEADERS, MemberId::Peers),
        (BULK_BLOCK, trusted(0)),
    ];
    assert_eq!(sent, expected, "(ask, member) of every send in flight");
    let in_flight = core.members[&trusted(0)].in_flight;
    assert_eq!(in_flight, [0, 2, 1], "trusted 0 per lane (control, interactive, bulk)");
}

#[test]
#[should_panic(expected = "in flight within max_connections, reserves never borrowed (T1)")]
fn check_catches_in_flight_past_max_connections() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // trusted 0: 4 lane permits = control reserve + interactive 2 + bulk 1, all held
    member(&mut core, 0).in_flight[Lane::Interactive.index()] += 1;
    core.check();
}

#[test]
#[should_panic(expected = "in flight = open asks' sends (T4)")]
fn check_catches_in_flight_off_the_open_sends() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // trusted 0 carries TIP_BLOCK + LOOKUP on interactive: 2
    member(&mut core, 0).in_flight[Lane::Interactive.index()] = 1;
    core.check();
}

#[test]
#[should_panic(expected = "a send's member configured")]
fn check_catches_a_send_to_a_member_gone() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // PEER_HEADERS in flight there
    core.members.remove(&MemberId::Peers);
    core.check();
}

#[test]
#[should_panic(expected = "polls of one member at least 200 ms apart (T10)")]
fn check_catches_polls_closer_than_the_floor() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let poller = member(&mut core, 1).poller.as_mut().expect("trusted");
    poller.previous = poller.last.map(|last| last - Duration::from_millis(100));
    core.check();
}

#[test]
#[should_panic(expected = "a due poll is sent (T10)")]
fn check_catches_a_due_poll_never_sent() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    // live: due 1 s after its last start
    member(&mut core, 1).poller.as_mut().expect("trusted").last =
        Some(now - Duration::from_secs(9));
    core.check();
}

#[test]
#[should_panic(expected = "a trusted-only class never reaches the peer set (T2)")]
fn check_catches_a_trusted_only_class_sent_to_the_peer_set() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let interactive = Lane::Interactive.index();
    // LOOKUP's send moved trusted 0 → peers (route Any admits it, in flight + tried kept true)
    let lookup = ask(&mut core, LOOKUP);
    lookup.sends[0].member = MemberId::Peers;
    lookup.tried.insert(MemberId::Peers);
    member(&mut core, 0).in_flight[interactive] -= 1;
    core.members.get_mut(&MemberId::Peers).expect("configured").in_flight[interactive] += 1;
    core.check();
}

#[test]
#[should_panic(expected = "a send stays within its ask's route (T2)")]
fn check_catches_a_send_outside_its_route() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // in flight on trusted 0
    ask(&mut core, TIP_BLOCK).route = Route::Peers;
    core.check();
}

#[test]
#[should_panic(expected = "a round sends to each member once (T5)")]
fn check_catches_a_round_sending_to_a_member_twice() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // TIP_BLOCK sent to trusted 1 twice (in flight + tried kept true, room for both)
    let tip = ask(&mut core, TIP_BLOCK);
    let to_trusted_1 = Sent { member: trusted(1), ..tip.sends[0] };
    tip.sends.extend([to_trusted_1, to_trusted_1]);
    tip.tried.insert(trusted(1));
    member(&mut core, 1).in_flight[Lane::Interactive.index()] += 2;
    core.check();
}

#[test]
#[should_panic(expected = "every send's member in its round's tried (T5)")]
fn check_catches_a_send_missing_from_its_rounds_tried() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // trusted 0 forgotten: a re-ask could reach it again this round
    ask(&mut core, TIP_BLOCK).tried.clear();
    core.check();
}

#[test]
#[should_panic(expected = "a trusted validator to ask")]
fn a_core_without_trusted_validators_panics() {
    TrafficCore::new(&[], None, Instant::now());
}

#[test]
#[should_panic(expected = "at most TrustedValidatorId::MAX trusted validators")]
fn a_core_past_the_validator_limit_panics() {
    let limits = Limits::new(Limits::MIN_CONNECTIONS).expect("MIN_CONNECTIONS");
    TrafficCore::new(&[(0, limits); TrustedValidatorId::MAX + 1], None, Instant::now());
}

#[test]
#[should_panic(expected = "the peer set's in flight covers every lane reserve")]
fn peer_permits_short_of_the_reserves_panic() {
    Permits::peers(crate::class::LANES as u32 - 1);
}

#[test]
#[should_panic(expected = "time never runs back")]
fn a_step_back_in_time_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let before = core.now - Duration::from_millis(1);
    core.step(Input::Tick, before);
}

#[test]
#[should_panic(expected = "a fresh ask id")]
fn an_ask_id_reused_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    core.step(Input::Ask { ask: TIP_BLOCK, class: Class::Lookup, route: Route::Any }, now);
}

#[test]
#[should_panic(expected = "a reply for a send in flight")]
fn a_reply_without_a_send_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    // TIP_BLOCK went to trusted 0, never 1
    let ticket = Ticket { ask: TIP_BLOCK, member: trusted(1), class: Class::TipBlock };
    core.step(Input::Reply { ticket, reply: Reply::Value }, now);
}

#[test]
#[should_panic(expected = "abandon: an open ask")]
fn abandoning_an_unknown_ask_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    core.step(Input::Abandon(AskId(9)), now);
}

#[test]
#[should_panic(expected = "a poll result for a poll in flight")]
fn a_poll_result_without_a_poll_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    // trusted 0's poll answered by the fixture, the next not yet due
    let validator = TrustedValidatorId::new(0).expect("small");
    core.step(Input::Polled { validator, read: None }, now);
}

#[test]
#[should_panic(expected = "report: a configured validator")]
fn a_report_on_an_unknown_validator_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    // only 0 and 1 configured
    let ticket = Ticket { ask: TIP_BLOCK, member: trusted(5), class: Class::TipBlock };
    core.step(Input::Report(ticket), now);
}

#[test]
#[should_panic(expected = "health: a configured validator")]
fn the_health_of_an_unknown_validator_panics() {
    let core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    // only 0 and 1 configured
    core.health(TrustedValidatorId::new(5).expect("small"));
}

#[test]
#[should_panic(expected = "push: a configured validator")]
fn a_push_from_an_unknown_validator_panics() {
    let mut core = two_live_trusted_and_peers_with_four_asks_open(Instant::now());
    let now = core.now;
    // only 0 and 1 configured
    let validator = TrustedValidatorId::new(5).expect("small");
    core.step(Input::Push { validator, push: Push::Changed }, now);
}
