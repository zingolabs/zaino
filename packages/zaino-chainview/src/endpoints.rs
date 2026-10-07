//! Who the endpoints are, and what the view needs to know about each

use std::time::Instant;

use zaino_primitives::types::{BlockRef, BlockchainInfo, EndOfService, NodeRelease, PeerInfo};
use zaino_traffic::{Health, ValidatorId};

/// Which endpoints, by [`ValidatorId`]: a bitset, never a count
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointSet(u64);

const _: () = assert!(ValidatorId::MAX <= EndpointSet::MAX, "every validator has a bit");

impl EndpointSet {
    /// Ceiling on the configured set (the bitset's width)
    pub const MAX: usize = u64::BITS as usize;

    pub(crate) fn insert(&mut self, endpoint: ValidatorId) {
        self.0 |= 1u64 << endpoint.get();
    }

    pub(crate) fn remove(&mut self, endpoint: ValidatorId) {
        self.0 &= !(1u64 << endpoint.get());
    }

    pub(crate) fn contains(&self, endpoint: ValidatorId) -> bool {
        self.0 & (1u64 << endpoint.get()) != 0
    }

    pub fn count(&self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Every endpoint in `other` is in `self`
    pub fn covers(&self, other: EndpointSet) -> bool {
        self.0 & other.0 == other.0
    }

    /// `self` minus every endpoint in `other`
    pub fn without(self, other: EndpointSet) -> EndpointSet {
        EndpointSet(self.0 & !other.0)
    }

    /// Positions in the configured endpoint list, ascending
    pub fn positions(self) -> impl Iterator<Item = usize> {
        (0..Self::MAX).filter(move |position| self.0 & (1u64 << position) != 0)
    }

    /// Endpoints at `positions` in the configured list (each `< ValidatorId::MAX`, asserted)
    pub fn at(positions: impl IntoIterator<Item = usize>) -> Self {
        positions
            .into_iter()
            .map(|position| {
                ValidatorId::new(position)
                    .unwrap_or_else(|| panic!("endpoint {position} past {}", ValidatorId::MAX))
            })
            .collect()
    }
}

impl FromIterator<ValidatorId> for EndpointSet {
    fn from_iter<I: IntoIterator<Item = ValidatorId>>(endpoints: I) -> Self {
        let mut set = Self::default();
        for endpoint in endpoints {
            set.insert(endpoint);
        }
        set
    }
}

/// Its chain vs the verified best block, as of its last answered poll (`verified-chain.md` §7)
///
/// - `Ahead` = holds best, claims higher
/// - `Behind` = its claim on the verified chain, below best
/// - `Diverged` = neither (a losing branch: an alarm, never an error, zebra #11133)
/// - `Unknown` = nothing verified yet, or no answer since its last failure
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Agreement {
    #[default]
    Unknown,
    Agreed,
    Ahead,
    Behind,
    Diverged,
}

impl Agreement {
    /// Metric label + status text
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Agreed => "agreed",
            Self::Ahead => "ahead",
            Self::Behind => "behind",
            Self::Diverged => "diverged",
        }
    }
}

/// One configured validator as last observed (`peers` = telemetry only, never gates serving)
///
/// - `health` = the balancer's, as of its last poll (`Live` + `CatchingUp` answer: may hold a tip)
/// - `info` = its last `getblockchaininfo` (its tip, clock estimate, upgrade schedule, branch)
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatorMetadata {
    pub address: String,
    pub health: Health,
    pub agreement: Agreement,
    pub observed_at: Option<Instant>,
    pub peers: imbl::Vector<PeerInfo>,
    pub release: Option<NodeRelease>,
    pub streaming: bool,
    pub(crate) info: Option<BlockchainInfo>,
}

impl ValidatorMetadata {
    pub(crate) fn new(address: String) -> Self {
        Self {
            address,
            health: Health::Pending,
            agreement: Agreement::Unknown,
            observed_at: None,
            peers: imbl::Vector::new(),
            release: None,
            streaming: false,
            info: None,
        }
    }

    /// Blocks its own tip has left before its release halts (`None` = no halt known, or no tip)
    pub fn blocks_to_end_of_service(&self) -> Option<u32> {
        let EndOfService::At { height, .. } = self.release.as_ref()?.end_of_service else {
            return None;
        };
        Some(u32::from(height).saturating_sub(u32::from(self.tip()?.height)))
    }

    /// Its own last-observed tip, not the verified one (`agreement` says how they relate)
    pub fn tip(&self) -> Option<BlockRef> {
        let info = self.info.as_ref()?;
        Some(BlockRef { hash: info.best_block_hash, height: info.blocks })
    }

    /// Blocks its tip trails its own clock estimate by (an eclipsed or stalled node's tell)
    pub fn stale_blocks(&self) -> Option<u32> {
        let (tip, info) = (self.tip()?, self.info.as_ref()?);
        Some(u32::from(info.estimated_height).saturating_sub(u32::from(tip.height)))
    }
}
