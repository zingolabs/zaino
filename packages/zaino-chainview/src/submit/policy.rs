//! [`SubmitPolicy`] + `Job`: pure submission policy, relayed by attempts, watched until it spreads
//! (`docs/architecture.md` §6)
//!
//! ```text
//!   step ─▶ Push(Peers: zebra's relay to ~⅓ of the peers)  while budget left
//!     │     Push(Trusted: untried, random)                 no peers, or past the budget once
//!     │        accepted / delivered ─▶ Wait(push + threshold) ─▶ spread? done : next attempt
//!     │        rejected / unreachable ─▶ next attempt at once
//!     └─▶ Done: spread │ accepted, nothing left to observe │ rejected │ unreachable │ exhausted
//! ```
//!
//! - pure: time and observations come in, steps go out (`driver.rs` executes them)
//! - `checked` = a trusted `checkadmission` accepted it first (the wallet's answer, already given)
//! - peers first: a trusted validator relays it only past the budget, unspread (Dandelion++'s
//!   fail-safe)
//! - unchecked → that one trusted push = the verdict (a peer answers with nothing)

use std::num::NonZeroU8;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use zaino_source::{NonDomainError, SendRawTransactionError};

use zaino_traffic::TrustedValidatorId;

use crate::chain_view_snapshot::MempoolState;
use crate::trusted_validators::TrustedValidatorSet;

/// How hard Zaino pushes one transaction into the network (zainod's `[submission]`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SubmitPolicy {
    #[serde(rename = "propagation_threshold_secs", with = "zaino_primitives::serde_secs::nonzero")]
    pub propagation_threshold: Duration,
    pub max_attempts: NonZeroU8,
}

impl Default for SubmitPolicy {
    fn default() -> Self {
        Self {
            propagation_threshold: Duration::from_secs(15),
            max_attempts: const { NonZeroU8::new(4).expect("4 is non-zero") },
        }
    }
}

/// Where one attempt goes (`Peers` = zebra picks them, names none)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Entry {
    Peers,
    Trusted(TrustedValidatorId),
}

/// One push's answer
///
/// - `Accepted` = trusted validator admitted it
/// - `Delivered` = the peers took the bytes (a peer answers nothing either way)
#[derive(Debug)]
pub(crate) enum Pushed {
    Accepted,
    Delivered,
    Rejected(SendRawTransactionError),
    Unreachable(NonDomainError),
}

/// What the view shows of the transaction now
///
/// - `listed` = a trusted validator lists it, or a peer quorum admitted it (each verified it)
/// - `spread` = a trusted validator never pushed to lists it, or a peer quorum (> 2/3 of fresh
///   listings: past the ~⅓ one relay reaches)
/// - `observable` = a trusted validator never pushed to is read, or the peers are quorate (else
///   spread = unknowable)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) listed: bool,
    pub(crate) spread: bool,
    pub(crate) observable: bool,
}

impl Seen {
    /// - `listing` ⊆ `readers`
    /// - `quorum` = a peer quorum admitted it
    /// - `quorate` = the peers' listings can make one
    pub(crate) fn of(
        listing: TrustedValidatorSet,
        quorum: bool,
        readers: TrustedValidatorSet,
        quorate: bool,
        tried: &[Entry],
    ) -> Self {
        let mut tried_trusted = TrustedValidatorSet::default();
        for entry in tried {
            if let Entry::Trusted(index) = *entry {
                tried_trusted.insert(index);
            }
        }
        Self {
            listed: MempoolState::of(listing, quorum).listed(),
            spread: !listing.without(tried_trusted).is_empty() || quorum,
            observable: !readers.without(tried_trusted).is_empty() || quorate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Push(Entry),
    Wait(Instant),
    Done,
}

/// - `Spread` = accepted, then seen at a source never an entry
/// - `Unconfirmed` = accepted, every readable source an entry (or every attempt spent unseen)
#[derive(Debug)]
pub(crate) enum Ended {
    Spread,
    Unconfirmed,
    Rejected(SendRawTransactionError),
    Unreachable(Option<NonDomainError>),
}

/// One transaction's attempts
#[derive(Debug)]
pub(crate) struct Job {
    policy: SubmitPolicy,
    rng: fastrand::Rng,
    peers: bool,
    trusted: Vec<TrustedValidatorId>,
    tried: Vec<Entry>,
    deadline: Option<Instant>,
    accepted: bool,
    rejection: Option<SendRawTransactionError>,
    failure: Option<NonDomainError>,
    ended: bool,
}

impl Job {
    /// - `peers` = live peers to relay through (none = trusted entries only)
    /// - `checked` = a trusted `checkadmission` accepted it
    pub(crate) fn new(
        peers: bool,
        trusted: Vec<TrustedValidatorId>,
        checked: bool,
        policy: SubmitPolicy,
        rng: fastrand::Rng,
    ) -> Self {
        Self {
            policy,
            rng,
            peers,
            trusted,
            tried: Vec::new(),
            deadline: None,
            accepted: checked,
            rejection: None,
            failure: None,
            ended: false,
        }
    }

    /// Entries pushed to, in order
    pub(crate) fn tried(&self) -> &[Entry] {
        &self.tried
    }

    /// `true` once checked, or a trusted validator accepted or lists it (the wallet's answer)
    pub(crate) fn accepted(&self) -> bool {
        self.accepted
    }

    /// Next move, given what the view shows at `now`
    ///
    /// - never done unpushed while unspread (a checked transaction is still relayed)
    pub(crate) fn step(&mut self, now: Instant, seen: Seen) -> Step {
        if self.ended {
            return Step::Done;
        }
        self.accepted |= seen.listed;
        let unobservable = !self.tried.is_empty() && !seen.observable;
        if self.accepted && (seen.spread || unobservable) {
            return self.end();
        }
        if let Some(deadline) = self.deadline {
            if now < deadline {
                return Step::Wait(deadline);
            }
            self.deadline = None;
        }
        let budget = self.tried.len() < usize::from(self.policy.max_attempts.get());
        if budget && self.peers {
            return self.push(Entry::Peers);
        }
        let tried_trusted = self.tried.iter().any(|entry| matches!(entry, Entry::Trusted(_)));
        if (budget || !tried_trusted) && !self.trusted.is_empty() {
            let index = self.trusted.swap_remove(self.rng.usize(..self.trusted.len()));
            return self.push(Entry::Trusted(index));
        }
        self.end()
    }

    /// What the push to the last `Push` answered
    pub(crate) fn pushed(&mut self, outcome: Pushed, now: Instant) {
        match outcome {
            Pushed::Accepted => {
                self.accepted = true;
                self.deadline = Some(now + self.policy.propagation_threshold);
            }
            Pushed::Delivered => self.deadline = Some(now + self.policy.propagation_threshold),
            Pushed::Rejected(rejection) => {
                self.rejection.get_or_insert(rejection);
            }
            Pushed::Unreachable(failure) => self.failure = Some(failure),
        }
    }

    /// Once `step` returned `Done`
    pub(crate) fn ended(self, seen: Seen) -> Ended {
        match (self.accepted, self.rejection) {
            (true, _) if seen.spread => Ended::Spread,
            (true, _) => Ended::Unconfirmed,
            (false, Some(rejection)) => Ended::Rejected(rejection),
            (false, None) => Ended::Unreachable(self.failure),
        }
    }

    fn push(&mut self, entry: Entry) -> Step {
        self.tried.push(entry);
        Step::Push(entry)
    }

    fn end(&mut self) -> Step {
        self.ended = true;
        Step::Done
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::*;

    fn index(position: usize) -> TrustedValidatorId {
        TrustedValidatorId::new(position).expect("< MAX")
    }

    /// One simulated node's behaviour toward the transaction
    ///
    /// - `Relays` = gossips to every honest node `after` it took it
    /// - `BlackHoles` = never gossips (a peer: still lists it once pushed to)
    /// - `Rejects` (trusted) = domain rejection (stricter local policy, or a bad tx for it)
    /// - `Unread` (trusted) = running, mempool not read (catching up, down)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Node {
        Relays { after: Duration },
        BlackHoles,
        Rejects,
        Unreachable,
        Unread,
    }

    fn relays() -> impl Strategy<Value = Node> {
        (0u64..30_000).prop_map(|ms| Node::Relays { after: Duration::from_millis(ms) })
    }

    fn trusted_node() -> impl Strategy<Value = Node> {
        prop_oneof![
            4 => relays(),
            1 => Just(Node::BlackHoles),
            1 => Just(Node::Rejects),
            1 => Just(Node::Unreachable),
            1 => Just(Node::Unread),
        ]
    }

    fn peer_node() -> impl Strategy<Value = Node> {
        prop_oneof![4 => relays(), 1 => Just(Node::BlackHoles)]
    }

    /// `peers_up` = a `Peers` push delivers (else no ready peer)
    #[derive(Debug, Clone)]
    struct Case {
        trusted: Vec<Node>,
        peers: Vec<Node>,
        peers_up: bool,
        checked: bool,
        threshold: Duration,
        max_attempts: u8,
        seed: u64,
    }

    fn case() -> impl Strategy<Value = Case> {
        (
            prop::collection::vec(trusted_node(), 1..5),
            prop::collection::vec(peer_node(), 0..9),
            any::<bool>(),
            any::<bool>(),
            (1_000u64..20_000).prop_map(Duration::from_millis),
            1u8..6,
            any::<u64>(),
        )
            .prop_map(
                |(trusted, peers, peers_up, checked, threshold, max_attempts, seed)| Case {
                    trusted,
                    peers,
                    peers_up,
                    checked,
                    threshold,
                    max_attempts,
                    seed,
                },
            )
    }

    /// Who holds the transaction at `at`, from who took it when (one gossip hop from a relaying
    /// holder reaches everyone honest, a second hop adds nobody)
    ///
    /// - one `Peers` push = ⌈n/3⌉ peers, the next ⌈n/3⌉ on each push (zebra samples at random)
    /// - every peer in its own netgroup: quorate from 6 peers, quorum = > 2/3 of them hold it
    struct Network {
        trusted: Vec<Node>,
        peers: Vec<Node>,
        took: BTreeMap<Holder, Instant>,
        relays: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Holder {
        Trusted(usize),
        Peer(usize),
    }

    impl Network {
        fn node(&self, holder: Holder) -> Node {
            match holder {
                Holder::Trusted(i) => self.trusted[i],
                Holder::Peer(i) => self.peers[i],
            }
        }

        fn holds(&self, holder: Holder, at: Instant) -> bool {
            let honest = !matches!(self.node(holder), Node::Unreachable | Node::Rejects);
            let direct = self.took.get(&holder).is_some_and(|&t| t <= at);
            let gossiped = honest
                && self.took.iter().any(|(from, &t)| {
                    matches!(self.node(*from), Node::Relays { after } if t + after <= at)
                });
            direct || gossiped
        }

        /// The peers the next `Peers` push reaches
        fn relay(&mut self, at: Instant) {
            let n = self.peers.len();
            let third = n.div_ceil(3);
            for k in 0..third {
                let peer = (self.relays * third + k) % n;
                self.took.entry(Holder::Peer(peer)).or_insert(at);
            }
            self.relays += 1;
        }

        fn seen(&self, tried: &[Entry], at: Instant) -> Seen {
            let readers: TrustedValidatorSet = (0..self.trusted.len())
                .filter(|&i| !matches!(self.trusted[i], Node::Unread))
                .map(index)
                .collect();
            let listing: TrustedValidatorSet = readers
                .positions()
                .filter(|&i| self.holds(Holder::Trusted(i), at))
                .map(index)
                .collect();
            let quorate = self.peers.len() >= 6;
            let holding = (0..self.peers.len()).filter(|&i| self.holds(Holder::Peer(i), at));
            let quorum = quorate && holding.count() * 3 > self.peers.len() * 2;
            Seen::of(listing, quorum, readers, quorate, tried)
        }
    }

    /// One push as the job made it: `at` since start, `waits` = its answer starts a threshold,
    /// `seen` + `accepted` = the job's view as it pushed
    #[derive(Debug, Clone, Copy)]
    struct Push {
        at: Duration,
        entry: Entry,
        waits: bool,
        seen: Seen,
        accepted: bool,
    }

    /// The job run to `Done` on a virtual clock (≤ 500 ms per wait step), times since start
    ///
    /// - `ran_away` = still running past `(max_attempts + 1) × threshold + 1 s` (stopped there)
    #[derive(Debug)]
    struct Run {
        pushes: Vec<Push>,
        waits: Vec<(Duration, Duration)>,
        ended_at: Duration,
        ran_away: bool,
        listed_ever: bool,
        accepted: bool,
        seen: Seen,
        ended: Ended,
    }

    impl Run {
        fn tried(&self) -> Vec<Entry> {
            self.pushes.iter().map(|push| push.entry).collect()
        }

        fn tried_trusted(&self) -> Vec<Entry> {
            let trusted = |entry: &Entry| matches!(entry, Entry::Trusted(_));
            self.tried().into_iter().filter(trusted).collect()
        }
    }

    fn bound(case: &Case) -> Duration {
        case.threshold * (u32::from(case.max_attempts) + 1) + Duration::from_secs(1)
    }

    fn run(case: &Case) -> Run {
        let policy = SubmitPolicy {
            propagation_threshold: case.threshold,
            max_attempts: NonZeroU8::new(case.max_attempts).expect("≥ 1"),
        };
        let start = Instant::now();
        let mut now = start;
        let (trusted, peers) = (case.trusted.clone(), case.peers.clone());
        let mut network = Network { trusted, peers, took: BTreeMap::new(), relays: 0 };
        let entries = (0..case.trusted.len()).map(index).collect();
        let rng = fastrand::Rng::with_seed(case.seed);
        let mut job = Job::new(!case.peers.is_empty(), entries, case.checked, policy, rng);
        let (mut pushes, mut waits, mut listed_ever, mut ran_away) = (vec![], vec![], false, false);
        loop {
            let seen = network.seen(job.tried(), now);
            listed_ever |= seen.listed;
            match job.step(now, seen) {
                Step::Push(entry) => {
                    let outcome = match entry {
                        Entry::Peers if case.peers_up => {
                            network.relay(now);
                            Pushed::Delivered
                        }
                        Entry::Peers => Pushed::Unreachable(NonDomainError::new(
                            zaino_source::FailureMode::Timeout,
                            "no ready peer",
                        )),
                        Entry::Trusted(i) => match case.trusted[i.get()] {
                            Node::Unreachable => Pushed::Unreachable(NonDomainError::new(
                                zaino_source::FailureMode::Connection,
                                "down",
                            )),
                            Node::Rejects => {
                                let message = "policy".into();
                                Pushed::Rejected(SendRawTransactionError::Rejected {
                                    code: -26,
                                    message,
                                })
                            }
                            _ => {
                                network.took.insert(Holder::Trusted(i.get()), now);
                                Pushed::Accepted
                            }
                        },
                    };
                    let waits = matches!(outcome, Pushed::Accepted | Pushed::Delivered);
                    let accepted = job.accepted();
                    pushes.push(Push { at: now - start, entry, waits, seen, accepted });
                    job.pushed(outcome, now);
                }
                Step::Wait(until) => {
                    waits.push((now - start, until - start));
                    now = until.min(now + Duration::from_millis(500));
                }
                Step::Done => break,
            }
            if now - start > bound(case) {
                ran_away = true;
                break;
            }
        }
        let seen = network.seen(job.tried(), now);
        let accepted = job.accepted();
        let ended = job.ended(seen);
        Run { pushes, waits, ended_at: now - start, ran_away, listed_ever, accepted, seen, ended }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, ..ProptestConfig::default() })]

        /// Privacy: the budget all relays through the peers, ≤ 1 trusted push past it, none twice
        #[test]
        fn a_job_relays_through_peers_within_its_budget_and_pushes_one_trusted_validator_past_it(
            case in case(),
        ) {
            let run = run(&case);

            let (tried, trusted_tried) = (run.tried(), run.tried_trusted());
            let distinct: BTreeSet<Entry> = trusted_tried.iter().copied().collect();
            let once = distinct.len() == trusted_tried.len();
            prop_assert!(once, "a trusted validator pushed twice: {:?}", tried);
            let budget = usize::from(case.max_attempts);
            let bounded = tried.len() <= budget + 1;
            prop_assert!(bounded, "{} pushes, budget {}: {:?}", tried.len(), budget, tried);
            if !case.peers.is_empty() {
                let peers_first = tried.iter().take(budget).all(|entry| *entry == Entry::Peers);
                prop_assert!(peers_first, "with peers, the budget all via them: {:?}", tried);
                prop_assert!(trusted_tried.len() <= 1, "one trusted push at most: {:?}", tried);
            }
            if let Some(past) = tried.get(budget) {
                let trusted = matches!(past, Entry::Trusted(_));
                prop_assert!(trusted, "past the budget only a trusted push: {:?}", tried);
            }
        }

        /// Patience: no push inside a waiting push's threshold or once spread and accepted, a
        /// checked one still relayed, the job ended within its attempts
        #[test]
        fn a_job_waits_out_each_threshold_stops_once_spread_and_ends_within_its_attempts(
            case in case(),
        ) {
            let run = run(&case);

            let (ended_at, bound) = (run.ended_at, bound(&case));
            prop_assert!(!run.ran_away, "still running at {:?}, bound {:?}", ended_at, bound);
            for pair in run.pushes.windows(2) {
                let (before, push) = (pair[0], pair[1]);
                let waited = !before.waits || push.at >= before.at + case.threshold;
                let threshold = case.threshold;
                prop_assert!(
                    waited,
                    "{:?} inside the {:?} threshold of {:?}", push, threshold, before
                );
            }
            for push in &run.pushes {
                let spread = push.seen.spread && push.accepted;
                prop_assert!(!spread, "pushed once spread and accepted: {:?}", push);
            }
            for (at, until) in &run.waits {
                prop_assert!(until > at, "a wait at {:?} until {:?}: not in the future", at, until);
            }
            if case.checked && !run.seen.spread {
                prop_assert!(!run.pushes.is_empty(), "checked, unspread, never relayed: {:?}", run);
            }
        }

        /// Answer: accepted iff checked, a trusted entry took it or it was listed; a rejection only
        /// from a trusted validator; unreachable only without one
        #[test]
        fn a_jobs_answer_comes_from_a_check_a_trusted_entry_or_a_listing(case in case()) {
            let run = run(&case);

            let node = |entry: &Entry| match entry {
                Entry::Trusted(i) => Some(case.trusted[i.get()]),
                Entry::Peers => None,
            };
            let nodes: Vec<Node> = run.tried_trusted().iter().filter_map(node).collect();
            let trusted_took =
                nodes.iter().any(|node| !matches!(node, Node::Rejects | Node::Unreachable));
            let trusted_rejected = nodes.contains(&Node::Rejects);
            let earned = case.checked || trusted_took || run.listed_ever;
            prop_assert_eq!(
                run.accepted, earned,
                "accepted vs (checked {} || a trusted entry took it {} || listed {}): {:?}",
                case.checked, trusted_took, run.listed_ever, run.tried()
            );
            let asked_trusted = !nodes.is_empty();
            let tried = run.tried();
            let answered = run.accepted || asked_trusted;
            prop_assert!(answered, "refused, no trusted validator asked: {:?}", tried);
            let answer = match &run.ended {
                Ended::Spread => run.accepted && run.seen.spread,
                Ended::Unconfirmed => run.accepted,
                Ended::Rejected(_) => !run.accepted && trusted_rejected,
                Ended::Unreachable(_) => !run.accepted && !trusted_rejected,
            };
            prop_assert!(
                answer,
                "{:?} with accepted {}, spread {}, a trusted rejection {}",
                run.ended, run.accepted, run.seen.spread, trusted_rejected
            );
        }

        /// A trusted entry relaying within the threshold (+ one 500 ms clock step), an honest
        /// reader never pushed to: it lists the transaction in time
        #[test]
        fn a_fast_relay_with_an_honest_reader_outside_the_entries_ends_spread(case in case()) {
            let run = run(&case);

            let tried = run.tried();
            let fast_trusted = tried.iter().any(|entry| matches!(entry, Entry::Trusted(i)
                if matches!(case.trusted[i.get()], Node::Relays { after }
                    if after + Duration::from_millis(500) < case.threshold)));
            let reader_outside = (0..case.trusted.len()).any(|i| {
                matches!(case.trusted[i], Node::Relays { .. } | Node::BlackHoles)
                    && !tried.contains(&Entry::Trusted(index(i)))
            });
            if fast_trusted && reader_outside {
                let spread = matches!(run.ended, Ended::Spread);
                prop_assert!(spread, "ended {:?}, tried {:?}", run.ended, tried);
            }
        }
    }

    struct Row {
        case: &'static str,
        listing: &'static [usize],
        quorum: bool,
        readers: &'static [usize],
        quorate: bool,
        tried: Vec<Entry>,
        seen: Seen,
    }

    #[test]
    fn spread_counts_only_sources_that_were_never_an_entry() {
        let set = |of: &[usize]| of.iter().map(|&i| index(i)).collect::<TrustedValidatorSet>();
        let rows = [
            Row {
                case: "nothing yet",
                listing: &[],
                quorum: false,
                readers: &[0, 1],
                quorate: false,
                tried: vec![Entry::Peers],
                seen: Seen { listed: false, spread: false, observable: true },
            },
            Row {
                case: "peer quorum, every trusted validator down",
                listing: &[],
                quorum: true,
                readers: &[],
                quorate: true,
                tried: vec![Entry::Peers],
                seen: Seen { listed: true, spread: true, observable: true },
            },
            Row {
                case: "trusted entry lists, alone",
                listing: &[0],
                quorum: false,
                readers: &[0, 1],
                quorate: false,
                tried: vec![Entry::Trusted(index(0))],
                seen: Seen { listed: true, spread: false, observable: true },
            },
            Row {
                case: "outside trusted lists",
                listing: &[0, 1],
                quorum: false,
                readers: &[0, 1],
                quorate: false,
                tried: vec![Entry::Trusted(index(0))],
                seen: Seen { listed: true, spread: true, observable: true },
            },
            Row {
                case: "every reader an entry, peers not quorate",
                listing: &[0],
                quorum: false,
                readers: &[0],
                quorate: false,
                tried: vec![Entry::Peers, Entry::Trusted(index(0))],
                seen: Seen { listed: true, spread: false, observable: false },
            },
            Row {
                case: "every reader an entry, peers quorate",
                listing: &[0],
                quorum: false,
                readers: &[0],
                quorate: true,
                tried: vec![Entry::Trusted(index(0))],
                seen: Seen { listed: true, spread: false, observable: true },
            },
            Row {
                case: "no readers, no peers",
                listing: &[],
                quorum: false,
                readers: &[],
                quorate: false,
                tried: vec![],
                seen: Seen { listed: false, spread: false, observable: false },
            },
        ];
        for row in rows {
            let seen =
                Seen::of(set(row.listing), row.quorum, set(row.readers), row.quorate, &row.tried);
            assert_eq!(seen, row.seen, "{}", row.case);
        }
    }

    /// 8,000 seeded jobs, no peers, 4 trusted validators: 2,000 expected each (σ ≈ 39)
    #[test]
    fn without_peers_the_first_attempt_is_a_uniform_trusted_validator() {
        let blind = Seen { listed: false, spread: false, observable: false };
        let mut first: BTreeMap<Entry, u32> = BTreeMap::new();

        for seed in 0..8_000 {
            let trusted = (0..4).map(index).collect();
            let rng = fastrand::Rng::with_seed(seed);
            let mut job = Job::new(false, trusted, true, SubmitPolicy::default(), rng);
            let step = job.step(Instant::now(), blind);
            let Step::Push(entry) = step else {
                panic!("seed {seed}: first step {step:?}, not a push")
            };
            *first.entry(entry).or_default() += 1;
        }

        assert_eq!(first.len(), 4, "every trusted validator drawn: {first:?}");
        let even = first.values().all(|&n| (1_800..=2_200).contains(&n));
        assert!(even, "first entries ≈ 2,000 each (±5σ): {first:?}");
    }
}
