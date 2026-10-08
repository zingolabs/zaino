//! Submission: one random entry per attempt, watched until it spreads (`chainview.md` §6)
//!
//! ```text
//!   step ─▶ Push(peer: untried, netgroup unused, random)   while budget left + not spread
//!     │     Push(trusted: untried, random)                  peers gone / spread / verdict due
//!     │        accepted / delivered ─▶ Wait(push + threshold) ─▶ spread? done : next attempt
//!     │        rejected / unreachable ─▶ next attempt at once
//!     └─▶ Done: spread │ accepted, nothing left to observe │ rejected │ unreachable │ exhausted
//! ```
//!
//! - pure: time and observations come in, steps go out (the driver in `view.rs` executes them)
//! - peers first (no trusted validator sees a transaction first); a peer answers with nothing,
//!   so a trusted validator's `sendrawtransaction` = the verdict, one push past the budget
//! - wallet answer = first acceptance (a trusted entry's, or any trusted listing); a rejection or
//!   a failure only once every attempt is spent (one validator's refusal may be local policy)
//! - Dandelion++'s originator rule (one stem hop picked by the sender) + its fail-safe timer

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU8;
use std::time::Duration;

use imbl::OrdSet;
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

impl Default for SubmitPolicy {
    fn default() -> Self {
        Self {
            propagation_threshold: Duration::from_secs(15),
            max_attempts: NonZeroU8::new(4).expect("4 is non-zero"),
        }
    }
}

/// Where one attempt goes
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Entry {
    Peer(SocketAddr),
    Trusted(ValidatorId),
}

/// Address prefix one operator plausibly controls: /16 IPv4, /32 IPv6 (Bitcoin Core's buckets)
///
/// - IPv4-mapped IPv6 = IPv4 (one host, two spellings)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Netgroup {
    V4([u8; 2]),
    V6([u8; 4]),
}

impl Netgroup {
    pub(crate) fn of(addr: SocketAddr) -> Self {
        let v4 = |[a, b, ..]: [u8; 4]| Self::V4([a, b]);
        match addr.ip() {
            IpAddr::V4(ip) => v4(ip.octets()),
            IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                Some(mapped) => v4(mapped.octets()),
                None => {
                    let [a, b, c, d, ..] = ip.octets();
                    Self::V6([a, b, c, d])
                }
            },
        }
    }
}

/// One push's answer: `Accepted` = trusted validator admitted it; `Delivered` = a peer took the
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
/// - `listed` = any trusted validator lists it (each verified it)
/// - `spread` = some source never an entry has it
/// - `observable` = some source never an entry is being read (else spread = unknowable)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) listed: bool,
    pub(crate) spread: bool,
    pub(crate) observable: bool,
}

impl Seen {
    /// `listing` ⊆ `readers`; `announcers` = peers that announced it; `live` = peers heard now
    ///
    /// - a peer entry's own announcement = nothing (a black hole can echo it to us alone)
    /// - trusted entry's p2p address unknown: once one tried, peers need two announcers
    pub(crate) fn of(
        listing: EndpointSet,
        readers: EndpointSet,
        announcers: &OrdSet<SocketAddr>,
        live: &OrdSet<SocketAddr>,
        tried: &[Entry],
    ) -> Self {
        let mut tried_trusted = EndpointSet::default();
        let mut tried_peers = OrdSet::new();
        for entry in tried {
            match *entry {
                Entry::Trusted(index) => tried_trusted.insert(index),
                Entry::Peer(addr) => drop(tried_peers.insert(addr)),
            }
        }
        let needed = if tried_trusted.is_empty() { 1 } else { 2 };
        let outsiders = announcers.iter().filter(|peer| !tried_peers.contains(*peer)).count();
        Self {
            listed: !listing.is_empty(),
            spread: !listing.without(tried_trusted).is_empty() || outsiders >= needed,
            observable: !readers.without(tried_trusted).is_empty()
                || live.iter().any(|peer| !tried_peers.contains(peer)),
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
    peers: Vec<SocketAddr>,
    trusted: Vec<ValidatorId>,
    tried: Vec<Entry>,
    used: Vec<Netgroup>,
    deadline: Option<Instant>,
    accepted: bool,
    rejection: Option<SendRawTransactionError>,
    failure: Option<NonDomainError>,
    ended: bool,
}

impl Job {
    /// `peers` = entry candidates (none = trusted entries only, §6's form before peers)
    pub(crate) fn new(
        peers: Vec<SocketAddr>,
        trusted: Vec<ValidatorId>,
        policy: SubmitPolicy,
        rng: fastrand::Rng,
    ) -> Self {
        Self {
            policy,
            rng,
            peers,
            trusted,
            tried: Vec::new(),
            used: Vec::new(),
            deadline: None,
            accepted: false,
            rejection: None,
            failure: None,
            ended: false,
        }
    }

    /// Entries pushed to, in order
    pub(crate) fn tried(&self) -> &[Entry] {
        &self.tried
    }

    /// `true` once a trusted validator accepted or lists it (the wallet's answer)
    pub(crate) fn accepted(&self) -> bool {
        self.accepted
    }

    /// Next move, given what the view shows at `now`
    pub(crate) fn step(&mut self, now: Instant, seen: Seen) -> Step {
        if self.ended {
            return Step::Done;
        }
        self.accepted |= seen.listed;
        if self.accepted && (seen.spread || !seen.observable) {
            return self.end();
        }
        if let Some(deadline) = self.deadline {
            if now < deadline {
                return Step::Wait(deadline);
            }
            self.deadline = None;
        }
        let budget = self.tried.len() < usize::from(self.policy.max_attempts.get());
        if budget && !seen.spread {
            if let Some(peer) = self.sample_peer() {
                return self.push(Entry::Peer(peer));
            }
        }
        let tried_trusted = self.tried.iter().any(|entry| matches!(entry, Entry::Trusted(_)));
        let verdict_due = !self.accepted && !tried_trusted;
        if (budget || verdict_due) && !self.trusted.is_empty() {
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

    /// Uniform over untried peers outside every used netgroup
    fn sample_peer(&mut self) -> Option<SocketAddr> {
        let open: Vec<usize> = (0..self.peers.len())
            .filter(|&at| !self.used.contains(&Netgroup::of(self.peers[at])))
            .collect();
        if open.is_empty() {
            return None;
        }
        let peer = self.peers.swap_remove(open[self.rng.usize(..open.len())]);
        self.used.push(Netgroup::of(peer));
        Some(peer)
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
    use std::net::{Ipv4Addr, Ipv6Addr};

    use proptest::prelude::*;

    use super::*;

    /// One simulated node's behaviour toward the transaction
    ///
    /// - `Relays` = gossips to every honest node after `delay_ms` (a peer also announces it)
    /// - `BlackHoles` = never gossips; `Echoes` (peer) = never gossips, announces to us alone
    /// - `Rejects` (trusted) = domain rejection (stricter local policy, or a bad tx for it)
    /// - `Unread` (trusted) = running, mempool not read (catching up, down)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Node {
        Relays { delay_ms: u64 },
        BlackHoles,
        Echoes,
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
            1 => Just(Node::Echoes),
            1 => Just(Node::Unreachable),
        ]
    }

    /// Peers drawn from 3 /16s (collisions on purpose) + IPv4-mapped IPv6
    fn peer_addr() -> impl Strategy<Value = SocketAddr> {
        prop_oneof![
            (0u8..3, any::<u8>(), 1u16..)
                .prop_map(|(b, d, port)| { SocketAddr::from((Ipv4Addr::new(10, b, 0, d), port)) }),
            (0u8..3, any::<u8>(), 1u16..).prop_map(|(b, d, port)| {
                SocketAddr::from((Ipv4Addr::new(10, b, 1, d).to_ipv6_mapped(), port))
            }),
        ]
    }

    /// Who holds the transaction at `at`, from who took it when (one gossip hop from a relaying
    /// holder reaches everyone honest; a second hop adds nobody)
    struct World {
        trusted: Vec<Node>,
        peers: BTreeMap<SocketAddr, Node>,
        took: BTreeMap<Entry, Instant>,
    }

    impl World {
        fn holds(&self, entry: Entry, node: Node, at: Instant) -> bool {
            let honest = !matches!(node, Node::Unreachable | Node::Rejects);
            let direct = self.took.get(&entry).is_some_and(|&t| t <= at);
            let gossiped = honest
                && self.took.iter().any(|(from, &t)| {
                    matches!(self.node(*from), Node::Relays { delay_ms }
                        if t + Duration::from_millis(delay_ms) <= at)
                });
            direct || gossiped
        }

        fn node(&self, entry: Entry) -> Node {
            match entry {
                Entry::Trusted(index) => self.trusted[index.get()],
                Entry::Peer(addr) => self.peers[&addr],
            }
        }

        fn seen(&self, tried: &[Entry], at: Instant) -> Seen {
            let index = |i: usize| ValidatorId::new(i).expect("< MAX");
            let readers: EndpointSet = (0..self.trusted.len())
                .filter(|&i| !matches!(self.trusted[i], Node::Unread))
                .map(index)
                .collect();
            let listing: EndpointSet = readers
                .positions()
                .filter(|&i| self.holds(Entry::Trusted(index(i)), self.trusted[i], at))
                .map(index)
                .collect();
            let live: OrdSet<SocketAddr> = self
                .peers
                .iter()
                .filter(|(_, node)| **node != Node::Unreachable)
                .map(|(addr, _)| *addr)
                .collect();
            let announcers: OrdSet<SocketAddr> = self
                .peers
                .iter()
                .filter(|(_, node)| matches!(node, Node::Relays { .. } | Node::Echoes))
                .filter(|(addr, node)| self.holds(Entry::Peer(**addr), **node, at))
                .map(|(addr, _)| *addr)
                .collect();
            Seen::of(listing, readers, &announcers, &live, tried)
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, ..ProptestConfig::default() })]

        /// Simulated peers + trusted validators (every `Node`), virtual clock until `Done`
        ///
        /// - privacy: any peer candidate → first entry a peer; peer entries never share a
        ///   netgroup; no entry twice
        /// - bounded: <= max_attempts pushes, + 1 only for the trusted verdict (none tried,
        ///   nothing accepted); ends within (max_attempts + 1) × threshold
        /// - patience: no push inside a delivered / accepted push's threshold; none once spread
        ///   + accepted
        /// - answer: accepted iff a trusted entry accepted or a trusted reader listed it;
        ///   rejected only from a trusted rejection; unreachable only when no trusted answered
        /// - fast honest relay pushed + honest reader outside the entries = spread
        #[test]
        fn a_job_samples_peers_first_waits_out_each_threshold_and_ends_on_a_trusted_verdict(
            trusted in prop::collection::vec(trusted_node(), 1..5),
            peers in prop::collection::btree_map(peer_addr(), peer_node(), 0..8),
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
            let mut world = World { trusted: trusted.clone(), peers: peers.clone(), took: BTreeMap::new() };
            let mut job = Job::new(
                peers.keys().copied().collect(),
                (0..trusted.len()).map(index).collect(),
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
                        prop_assert!(!pushes.iter().any(|(_, e, _)| *e == entry), "entry reused");
                        if let Some((at, _, waits)) = pushes.last() {
                            prop_assert!(!*waits || now >= *at + policy.propagation_threshold,
                                "resubmitted inside the threshold");
                        }
                        prop_assert!(!(observed.spread && job.accepted()), "pushed after spreading");
                        let outcome = match (entry, world.node(entry)) {
                            (_, Node::Unreachable) => Pushed::Unreachable(NonDomainError::new(
                                zaino_source::FailureMode::Connection, "down")),
                            (Entry::Trusted(_), Node::Rejects) => {
                                let policy = "policy".into();
                                Pushed::Rejected(SendRawTransactionError::Rejected { code: -26, message: policy })
                            }
                            (Entry::Trusted(_), _) => Pushed::Accepted,
                            (Entry::Peer(_), _) => Pushed::Delivered,
                        };
                        if !matches!(outcome, Pushed::Unreachable(_) | Pushed::Rejected(_)) {
                            world.took.insert(entry, now);
                        }
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
            if !peers.is_empty() {
                prop_assert!(matches!(tried.first(), Some(Entry::Peer(_))), "a peer goes first");
            }
            let groups: Vec<Netgroup> = tried.iter().filter_map(|e| match e {
                Entry::Peer(addr) => Some(Netgroup::of(*addr)),
                Entry::Trusted(_) => None,
            }).collect();
            prop_assert_eq!(groups.len(), groups.iter().collect::<BTreeSet<_>>().len(), "netgroup reused");
            let budget = usize::from(max_attempts);
            prop_assert!(tried.len() <= budget + 1);
            if tried.len() == budget + 1 {
                let last_is_first_trusted = matches!(tried[budget], Entry::Trusted(_))
                    && tried[..budget].iter().all(|e| matches!(e, Entry::Peer(_)));
                prop_assert!(last_is_first_trusted, "only the verdict goes past the budget: {:?}", tried);
            }

            let trusted_took = tried.iter().any(|e| matches!(e, Entry::Trusted(i)
                if !matches!(trusted[i.get()], Node::Rejects | Node::Unreachable)));
            let trusted_rejected = tried.iter().any(|e| matches!(e, Entry::Trusted(i)
                if trusted[i.get()] == Node::Rejects));
            let accepted = job.accepted();
            prop_assert_eq!(accepted, trusted_took || listed_ever);
            prop_assert!(accepted || tried.iter().any(|e| matches!(e, Entry::Trusted(_))),
                "a refusal or failure answered without asking a trusted validator: {:?}", tried);
            let final_seen = world.seen(&tried, now);
            let ended = job.ended(final_seen);
            match &ended {
                Ended::Spread => prop_assert!(accepted && final_seen.spread),
                Ended::Unconfirmed => prop_assert!(accepted),
                Ended::Rejected(_) => prop_assert!(!accepted && trusted_rejected),
                Ended::Unreachable(_) => prop_assert!(!accepted && !trusted_rejected),
            }

            let fast_relay = tried.iter().any(|e| {
                matches!(world.node(*e), Node::Relays { delay_ms } if delay_ms + 500 < threshold_ms)
            });
            let honest_reader_outside = (0..trusted.len()).any(|i| {
                matches!(trusted[i], Node::Relays { .. } | Node::BlackHoles)
                    && !tried.contains(&Entry::Trusted(index(i)))
            });
            if fast_relay && honest_reader_outside {
                prop_assert!(matches!(ended, Ended::Spread),
                    "a fast relay with an honest reader outside = spread, got {:?}", ended);
            }
        }
    }

    /// `Seen::of` case by case
    ///
    /// - entries never count as spread; a peer entry's echo = nothing
    /// - tried trusted entry (p2p address unknown) → two outside announcers needed
    #[test]
    fn spread_counts_only_sources_that_were_never_an_entry() {
        let t = |i: usize| ValidatorId::new(i).expect("< MAX");
        let p = |n: u8| SocketAddr::from(([10, n, 0, 1], 8233));
        let set = |of: &[usize]| of.iter().map(|&i| t(i)).collect::<EndpointSet>();
        let peers = |of: &[u8]| of.iter().map(|&n| p(n)).collect::<OrdSet<SocketAddr>>();
        let readers = set(&[0, 1]);
        struct Case {
            name: &'static str,
            listing: EndpointSet,
            announcers: OrdSet<SocketAddr>,
            live: OrdSet<SocketAddr>,
            tried: Vec<Entry>,
            want: Seen,
        }
        let seen = |listed, spread, observable| Seen { listed, spread, observable };
        let cases = [
            Case {
                name: "nothing yet",
                listing: set(&[]),
                announcers: peers(&[]),
                live: peers(&[1, 2]),
                tried: vec![Entry::Peer(p(1))],
                want: seen(false, false, true),
            },
            Case {
                name: "entry echo only",
                listing: set(&[]),
                announcers: peers(&[1]),
                live: peers(&[1, 2]),
                tried: vec![Entry::Peer(p(1))],
                want: seen(false, false, true),
            },
            Case {
                name: "outside peer announces",
                listing: set(&[]),
                announcers: peers(&[1, 2]),
                live: peers(&[1, 2]),
                tried: vec![Entry::Peer(p(1))],
                want: seen(false, true, true),
            },
            Case {
                name: "trusted entry lists, alone",
                listing: set(&[0]),
                announcers: peers(&[]),
                live: peers(&[]),
                tried: vec![Entry::Trusted(t(0))],
                want: seen(true, false, true),
            },
            Case {
                name: "outside trusted lists",
                listing: set(&[0, 1]),
                announcers: peers(&[]),
                live: peers(&[]),
                tried: vec![Entry::Trusted(t(0))],
                want: seen(true, true, true),
            },
            Case {
                name: "trusted entry + one announcer",
                listing: set(&[0]),
                announcers: peers(&[2]),
                live: peers(&[2]),
                tried: vec![Entry::Trusted(t(0)), Entry::Trusted(t(1))],
                want: seen(true, false, true),
            },
            Case {
                name: "every source an entry",
                listing: set(&[0]),
                announcers: peers(&[3, 4]),
                live: peers(&[1]),
                tried: vec![Entry::Peer(p(1)), Entry::Trusted(t(0)), Entry::Trusted(t(1))],
                want: seen(true, true, false),
            },
        ];
        for case in cases {
            let got = Seen::of(case.listing, readers, &case.announcers, &case.live, &case.tried);
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    /// - No peers → first entry uniform over the trusted validators
    /// - Peers → uniform over the peers, never a trusted validator (none singled out: §6's point)
    #[test]
    fn the_first_entry_is_uniform_and_a_peer_whenever_one_exists() {
        let index = |i: usize| ValidatorId::new(i).expect("< MAX");
        let peers: Vec<SocketAddr> =
            (0..4).map(|i| SocketAddr::from((Ipv4Addr::new(10, i, 0, 1), 8233))).collect();
        let seen = Seen { listed: false, spread: false, observable: true };
        for with_peers in [false, true] {
            let mut first: BTreeMap<String, u32> = BTreeMap::new();
            for seed in 0..8_000 {
                let candidates = if with_peers { peers.clone() } else { Vec::new() };
                let trusted = (0..4).map(index).collect();
                let mut job = Job::new(
                    candidates,
                    trusted,
                    SubmitPolicy::default(),
                    fastrand::Rng::with_seed(seed),
                );
                let Step::Push(entry) = job.step(Instant::now(), seen) else {
                    panic!("first step pushes")
                };
                assert_eq!(matches!(entry, Entry::Peer(_)), with_peers, "{entry:?}");
                *first.entry(format!("{entry:?}")).or_default() += 1;
            }
            assert_eq!(first.len(), 4, "every candidate drawn: {first:?}");
            assert!(
                first.values().all(|&n| (1_800..=2_200).contains(&n)),
                "≈ 2,000 each: {first:?}"
            );
        }

        let v4 = SocketAddr::from((Ipv4Addr::new(10, 1, 9, 9), 1));
        let mapped = SocketAddr::from((Ipv4Addr::new(10, 1, 2, 3).to_ipv6_mapped(), 2));
        assert_eq!(Netgroup::of(v4), Netgroup::of(mapped), "mapped v6 = its v4 /16");
        let v6 = |a: u16| SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, a, 0, 0, 0, 0, 1), 3));
        assert_eq!(Netgroup::of(v6(1)), Netgroup::V6([0x20, 0x01, 0x0d, 0xb8]));
        assert_eq!(Netgroup::of(v6(1)), Netgroup::of(v6(2)), "/32: same operator block");
    }
}
