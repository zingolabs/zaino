//! Submission: relayed by attempts, watched until it spreads (`docs/architecture.md` §6)
//!
//! ```text
//!   step ─▶ Push(Peers: zebra's relay to ~⅓ of the peers)  while budget left
//!     │     Push(Trusted: untried, random)                 no peers, or past the budget once
//!     │        accepted / delivered ─▶ Wait(push + threshold) ─▶ spread? done : next attempt
//!     │        rejected / unreachable ─▶ next attempt at once
//!     └─▶ Done: spread │ accepted, nothing left to observe │ rejected │ unreachable │ exhausted
//! ```
//!
//! - pure: time and observations come in, steps go out (the driver in `view.rs` executes them)
//! - `checked` = a trusted `checkadmission` accepted it first (the wallet's answer, already given)
//! - peers first: a trusted validator relays it only past the budget, unspread (Dandelion++'s
//!   fail-safe); unchecked, that one trusted push = the verdict (a peer answers with nothing)

use std::num::NonZeroU8;
use std::time::Duration;

use tokio::time::Instant;
use zaino_source::{NonDomainError, SendRawTransactionError};

use zaino_traffic::ValidatorId;

use crate::endpoints::EndpointSet;

/// How hard Zaino pushes one transaction into the network
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitPolicy {
    pub propagation_threshold: Duration,
    pub max_attempts: NonZeroU8,
}

impl SubmitPolicy {
    pub const DEFAULT: Self = Self {
        propagation_threshold: Duration::from_secs(15),
        max_attempts: NonZeroU8::new(4).expect("4 is non-zero"),
    };
}

impl Default for SubmitPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where one attempt goes (`Peers` = zebra picks them, names none)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Entry {
    Peers,
    Trusted(ValidatorId),
}

/// One push's answer: `Accepted` = trusted validator admitted it; `Delivered` = the peers took the
/// bytes (a peer answers nothing either way)
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
    /// `listing` ⊆ `readers`; `quorum` = a peer quorum admitted it; `quorate` = the peers'
    /// listings can make one
    pub(crate) fn of(
        listing: EndpointSet,
        quorum: bool,
        readers: EndpointSet,
        quorate: bool,
        tried: &[Entry],
    ) -> Self {
        let mut tried_trusted = EndpointSet::default();
        for entry in tried {
            if let Entry::Trusted(index) = *entry {
                tried_trusted.insert(index);
            }
        }
        Self {
            listed: !listing.is_empty() || quorum,
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
    trusted: Vec<ValidatorId>,
    tried: Vec<Entry>,
    deadline: Option<Instant>,
    accepted: bool,
    rejection: Option<SendRawTransactionError>,
    failure: Option<NonDomainError>,
    ended: bool,
}

impl Job {
    /// `peers` = live peers to relay through (none = trusted entries only); `checked` = a trusted
    /// `checkadmission` accepted it
    pub(crate) fn new(
        peers: bool,
        trusted: Vec<ValidatorId>,
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
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    /// One simulated node's behaviour toward the transaction
    ///
    /// - `Relays` = gossips to every honest node after `delay_ms`
    /// - `BlackHoles` = never gossips (a peer: still lists it once pushed to)
    /// - `Rejects` (trusted) = domain rejection (stricter local policy, or a bad tx for it)
    /// - `Unread` (trusted) = running, mempool not read (catching up, down)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Node {
        Relays { delay_ms: u64 },
        BlackHoles,
        Rejects,
        Unreachable,
        Unread,
    }

    fn trusted_node() -> impl Strategy<Value = Node> {
        prop_oneof![
            4 => (0u64..30_000).prop_map(|delay_ms| Node::Relays { delay_ms }),
            1 => Just(Node::BlackHoles),
            1 => Just(Node::Rejects),
            1 => Just(Node::Unreachable),
            1 => Just(Node::Unread),
        ]
    }

    fn peer_node() -> impl Strategy<Value = Node> {
        prop_oneof![
            4 => (0u64..30_000).prop_map(|delay_ms| Node::Relays { delay_ms }),
            1 => Just(Node::BlackHoles),
        ]
    }

    /// Who holds the transaction at `at`, from who took it when (one gossip hop from a relaying
    /// holder reaches everyone honest; a second hop adds nobody)
    ///
    /// - one `Peers` push = ⌈n/3⌉ peers, the next ⌈n/3⌉ on each push (zebra samples at random)
    /// - every peer in its own netgroup: quorate from 6 peers, quorum = > 2/3 of them hold it
    struct World {
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

    impl World {
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
                    matches!(self.node(*from), Node::Relays { delay_ms }
                        if t + Duration::from_millis(delay_ms) <= at)
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
            let index = |i: usize| ValidatorId::new(i).expect("< MAX");
            let readers: EndpointSet = (0..self.trusted.len())
                .filter(|&i| !matches!(self.trusted[i], Node::Unread))
                .map(index)
                .collect();
            let listing: EndpointSet = readers
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

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, ..ProptestConfig::default() })]

        /// Simulated peers + trusted validators (every `Node`), virtual clock until `Done`
        ///
        /// - privacy: with peers, the budget all relays through them; at most one trusted push,
        ///   past the budget; no trusted validator pushed twice
        /// - bounded: <= max_attempts pushes, + 1 only for that trusted push; ends within
        ///   (max_attempts + 1) × threshold
        /// - patience: no push inside a delivered / accepted push's threshold; none once spread
        ///   + accepted; a checked transaction pushed at least once
        /// - answer: accepted iff checked, a trusted entry accepted or it was listed; rejected only
        ///   from a trusted rejection; unreachable only when no trusted validator answered
        /// - fast honest relay pushed + honest reader outside the entries = spread
        #[test]
        fn a_job_relays_through_peers_first_waits_out_each_threshold_and_ends_on_a_trusted_verdict(
            trusted in prop::collection::vec(trusted_node(), 1..5),
            peers in prop::collection::vec(peer_node(), 0..9),
            peers_up in any::<bool>(),
            checked in any::<bool>(),
            threshold_ms in 1_000u64..20_000,
            max_attempts in 1u8..6,
            seed in any::<u64>(),
        ) {
            let policy = SubmitPolicy {
                propagation_threshold: Duration::from_millis(threshold_ms),
                max_attempts: NonZeroU8::new(max_attempts).expect("≥ 1"),
            };
            let index = |i: usize| ValidatorId::new(i).expect("< MAX");
            let start = Instant::now();
            let mut now = start;
            let mut world =
                World { trusted: trusted.clone(), peers: peers.clone(), took: BTreeMap::new(), relays: 0 };
            let has_peers = !peers.is_empty();
            let mut job = Job::new(
                has_peers,
                (0..trusted.len()).map(index).collect(),
                checked,
                policy,
                fastrand::Rng::with_seed(seed),
            );
            let mut pushes: Vec<(Instant, Entry, bool)> = Vec::new();
            let mut listed_ever = false;
            loop {
                let observed = world.seen(job.tried(), now);
                listed_ever |= observed.listed;
                match job.step(now, observed) {
                    Step::Push(entry) => {
                        if let Some((at, _, waits)) = pushes.last() {
                            prop_assert!(!*waits || now >= *at + policy.propagation_threshold,
                                "resubmitted inside the threshold");
                        }
                        prop_assert!(!(observed.spread && job.accepted()), "pushed after spreading");
                        let outcome = match entry {
                            Entry::Peers if peers_up => {
                                world.relay(now);
                                Pushed::Delivered
                            }
                            Entry::Peers => Pushed::Unreachable(NonDomainError::new(
                                zaino_source::FailureMode::Timeout, "no ready peer")),
                            Entry::Trusted(i) => match trusted[i.get()] {
                                Node::Unreachable => Pushed::Unreachable(NonDomainError::new(
                                    zaino_source::FailureMode::Connection, "down")),
                                Node::Rejects => {
                                    let policy = "policy".into();
                                    Pushed::Rejected(SendRawTransactionError::Rejected { code: -26, message: policy })
                                }
                                _ => {
                                    world.took.insert(Holder::Trusted(i.get()), now);
                                    Pushed::Accepted
                                }
                            },
                        };
                        let waits = matches!(outcome, Pushed::Accepted | Pushed::Delivered);
                        pushes.push((now, entry, waits));
                        job.pushed(outcome, now);
                    }
                    Step::Wait(until) => {
                        prop_assert!(until > now);
                        now = until.min(now + Duration::from_millis(500));
                    }
                    Step::Done => break,
                }
                let bound = policy.propagation_threshold * (u32::from(max_attempts) + 1);
                prop_assert!(now - start <= bound + Duration::from_secs(1), "ends within its attempts");
            }

            let tried: Vec<Entry> = pushes.iter().map(|(_, e, _)| *e).collect();
            let trusted_tried: Vec<Entry> =
                tried.iter().copied().filter(|e| matches!(e, Entry::Trusted(_))).collect();
            let distinct: std::collections::BTreeSet<Entry> = trusted_tried.iter().copied().collect();
            prop_assert_eq!(distinct.len(), trusted_tried.len(), "a trusted validator pushed twice");
            let budget = usize::from(max_attempts);
            prop_assert!(tried.len() <= budget + 1);
            if has_peers {
                prop_assert!(tried.iter().take(budget).all(|e| *e == Entry::Peers), "peers first: {:?}", tried);
                prop_assert!(trusted_tried.len() <= 1, "one trusted push at most: {:?}", tried);
            }
            if tried.len() == budget + 1 {
                prop_assert!(matches!(tried[budget], Entry::Trusted(_)), "only a trusted push past the budget");
            }
            let final_seen = world.seen(&tried, now);
            if checked && !final_seen.spread {
                prop_assert!(!tried.is_empty(), "checked, unspread, never relayed");
            }

            let trusted_took = tried.iter().any(|e| matches!(e, Entry::Trusted(i)
                if !matches!(trusted[i.get()], Node::Rejects | Node::Unreachable)));
            let trusted_rejected = tried.iter().any(|e| matches!(e, Entry::Trusted(i)
                if trusted[i.get()] == Node::Rejects));
            let accepted = job.accepted();
            prop_assert_eq!(accepted, checked || trusted_took || listed_ever);
            prop_assert!(accepted || !trusted_tried.is_empty(),
                "a refusal or failure answered without asking a trusted validator: {:?}", tried);
            let ended = job.ended(final_seen);
            match &ended {
                Ended::Spread => prop_assert!(accepted && final_seen.spread),
                Ended::Unconfirmed => prop_assert!(accepted),
                Ended::Rejected(_) => prop_assert!(!accepted && trusted_rejected),
                Ended::Unreachable(_) => prop_assert!(!accepted && !trusted_rejected),
            }

            let fast_trusted = tried.iter().any(|e| matches!(e, Entry::Trusted(i)
                if matches!(trusted[i.get()], Node::Relays { delay_ms } if delay_ms + 500 < threshold_ms)));
            let honest_reader_outside = (0..trusted.len()).any(|i| {
                matches!(trusted[i], Node::Relays { .. } | Node::BlackHoles)
                    && !tried.contains(&Entry::Trusted(index(i)))
            });
            if fast_trusted && honest_reader_outside {
                prop_assert!(matches!(ended, Ended::Spread),
                    "a fast relay with an honest reader outside = spread, got {:?}", ended);
            }
        }
    }

    /// `Seen::of` case by case
    ///
    /// - a trusted entry's own listing = listed, never spread; an untried trusted listing = both
    /// - a peer quorum = listed + spread, no trusted validator needed; quorate peers = observable
    #[test]
    fn spread_counts_only_sources_that_were_never_an_entry() {
        let t = |i: usize| ValidatorId::new(i).expect("< MAX");
        let set = |of: &[usize]| of.iter().map(|&i| t(i)).collect::<EndpointSet>();
        let readers = set(&[0, 1]);
        let seen = |listed, spread, observable| Seen { listed, spread, observable };
        type Case = (&'static str, EndpointSet, bool, EndpointSet, bool, Vec<Entry>, Seen);
        let cases: [Case; 7] = [
            (
                "nothing yet",
                set(&[]),
                false,
                readers,
                false,
                vec![Entry::Peers],
                seen(false, false, true),
            ),
            (
                "peer quorum, every trusted validator down",
                set(&[]),
                true,
                set(&[]),
                true,
                vec![Entry::Peers],
                seen(true, true, true),
            ),
            (
                "trusted entry lists, alone",
                set(&[0]),
                false,
                readers,
                false,
                vec![Entry::Trusted(t(0))],
                seen(true, false, true),
            ),
            (
                "outside trusted lists",
                set(&[0, 1]),
                false,
                readers,
                false,
                vec![Entry::Trusted(t(0))],
                seen(true, true, true),
            ),
            (
                "every reader an entry, peers not quorate",
                set(&[0]),
                false,
                set(&[0]),
                false,
                vec![Entry::Peers, Entry::Trusted(t(0))],
                seen(true, false, false),
            ),
            (
                "every reader an entry, peers quorate",
                set(&[0]),
                false,
                set(&[0]),
                true,
                vec![Entry::Trusted(t(0))],
                seen(true, false, true),
            ),
            (
                "no readers, no peers",
                set(&[]),
                false,
                set(&[]),
                false,
                vec![],
                seen(false, false, false),
            ),
        ];
        for (name, listing, quorum, readers, quorate, tried, want) in cases {
            assert_eq!(Seen::of(listing, quorum, readers, quorate, &tried), want, "{name}");
        }
    }

    /// - Peers → the first attempt relays through them, never a trusted validator
    /// - No peers → first entry uniform over the trusted validators
    /// - Checked + nothing observable yet → still a first push (never done unrelayed)
    #[test]
    fn the_first_attempt_is_the_peers_else_a_uniform_trusted_validator() {
        let index = |i: usize| ValidatorId::new(i).expect("< MAX");
        let blind = Seen { listed: false, spread: false, observable: false };
        for (peers, checked) in [(true, false), (true, true)] {
            let trusted = (0..4).map(index).collect();
            let rng = fastrand::Rng::with_seed(7);
            let mut job = Job::new(peers, trusted, checked, SubmitPolicy::default(), rng);
            assert_eq!(job.step(Instant::now(), blind), Step::Push(Entry::Peers), "{checked}");
        }
        let mut first: BTreeMap<String, u32> = BTreeMap::new();
        for seed in 0..8_000 {
            let trusted = (0..4).map(index).collect();
            let rng = fastrand::Rng::with_seed(seed);
            let mut job = Job::new(false, trusted, true, SubmitPolicy::default(), rng);
            let Step::Push(entry) = job.step(Instant::now(), blind) else {
                panic!("first step pushes")
            };
            *first.entry(format!("{entry:?}")).or_default() += 1;
        }
        assert_eq!(first.len(), 4, "every trusted validator drawn: {first:?}");
        assert!(first.values().all(|&n| (1_800..=2_200).contains(&n)), "≈ 2,000 each: {first:?}");
    }
}
