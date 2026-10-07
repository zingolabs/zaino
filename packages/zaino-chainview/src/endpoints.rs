//! Who the endpoints are, and what the view needs to know about each

use std::time::{Duration, Instant};

use zaino_primitives::types::{BlockRef, BlockchainInfo, EndOfService, NodeRelease, PeerInfo};

/// Position in the configured endpoint list, `< EndpointSet::MAX` (keeps `insert` infallible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EndpointIndex(u8);

impl EndpointIndex {
    pub(crate) fn new(index: usize) -> Option<Self> {
        (index < EndpointSet::MAX).then_some(Self(index as u8))
    }

    pub(crate) fn get(self) -> usize {
        usize::from(self.0)
    }
}

/// Which endpoints, by index: a bitset, never a count
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointSet(u64);

impl EndpointSet {
    /// Ceiling on the configured set (the bitset's width)
    pub const MAX: usize = u64::BITS as usize;

    pub(crate) fn insert(&mut self, endpoint: EndpointIndex) {
        self.0 |= 1u64 << endpoint.0;
    }

    pub(crate) fn remove(&mut self, endpoint: EndpointIndex) {
        self.0 &= !(1u64 << endpoint.0);
    }

    pub(crate) fn contains(&self, endpoint: EndpointIndex) -> bool {
        self.0 & (1u64 << endpoint.0) != 0
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

    /// Endpoints at `positions` in the configured list (each `< MAX`, asserted)
    pub fn at(positions: impl IntoIterator<Item = usize>) -> Self {
        positions
            .into_iter()
            .map(|position| {
                EndpointIndex::new(position)
                    .unwrap_or_else(|| panic!("endpoint {position} past {}", Self::MAX))
            })
            .collect()
    }
}

impl FromIterator<EndpointIndex> for EndpointSet {
    fn from_iter<I: IntoIterator<Item = EndpointIndex>>(endpoints: I) -> Self {
        let mut set = Self::default();
        for endpoint in endpoints {
            set.insert(endpoint);
        }
        set
    }
}

/// Where one endpoint stands with its poller (`Live` + `CatchingUp` answer, so can hold the tip)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EndpointState {
    /// No successful poll yet
    #[default]
    Pending,
    Live,
    /// Failing, on the backoff ladder (last observation retained)
    Degraded,
    /// Ejected: failure ceiling hit, or no mempool
    Down,
    /// Chain read, mempool off (node behind the network tip; an empty state reports genesis)
    CatchingUp,
}

impl EndpointState {
    /// Metric label + status text
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Live => "live",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::CatchingUp => "catching_up",
        }
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

/// Exponentially weighted mean round-trip time (fixed smoothing)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ewma {
    micros: Option<f64>,
}

impl Ewma {
    const ALPHA: f64 = 0.2;

    pub(crate) fn observe(&mut self, sample: Duration) {
        let sample = sample.as_micros() as f64;
        self.micros = Some(match self.micros {
            Some(mean) => mean + Self::ALPHA * (sample - mean),
            None => sample,
        });
    }

    /// `None` until the first observation
    pub fn mean(&self) -> Option<Duration> {
        self.micros.map(|micros| Duration::from_micros(micros as u64))
    }
}

/// One configured validator as last observed (`peers` = telemetry only, never gates serving)
///
/// - `info` = its last `getblockchaininfo` (its tip, clock estimate, upgrade schedule, branch)
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatorMetadata {
    pub address: String,
    pub state: EndpointState,
    pub agreement: Agreement,
    pub observed_at: Option<Instant>,
    pub latency: Ewma,
    pub failures: u32,
    pub peers: imbl::Vector<PeerInfo>,
    pub release: Option<NodeRelease>,
    /// Push stream up as of its last answered tick
    pub streaming: bool,
    pub(crate) info: Option<BlockchainInfo>,
}

impl ValidatorMetadata {
    pub(crate) fn new(address: String) -> Self {
        Self {
            address,
            state: EndpointState::Pending,
            agreement: Agreement::Unknown,
            observed_at: None,
            latency: Ewma::default(),
            failures: 0,
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
