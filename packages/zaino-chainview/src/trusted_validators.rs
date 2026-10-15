//! Who the trusted validators are, and what the view needs to know about each

use std::time::Instant;

use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::BlockRef;
use zaino_source::{BlockchainInfo, EndOfService, NodeRelease, PeerInfo};
use zaino_traffic::{Health, TrustedValidatorId};

/// Which trusted validators, by [`TrustedValidatorId`]: a bitset, never a count
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrustedValidatorSet(u64);

const _: () =
    assert!(TrustedValidatorId::MAX <= TrustedValidatorSet::MAX, "every validator has a bit");

impl TrustedValidatorSet {
    /// Ceiling on the configured set (the bitset's width)
    pub const MAX: usize = u64::BITS as usize;

    pub(crate) fn insert(&mut self, validator: TrustedValidatorId) {
        self.0 |= 1u64 << validator.get();
    }

    pub(crate) fn remove(&mut self, validator: TrustedValidatorId) {
        self.0 &= !(1u64 << validator.get());
    }

    pub fn contains(&self, validator: TrustedValidatorId) -> bool {
        self.0 & (1u64 << validator.get()) != 0
    }

    pub fn count(&self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Every validator in `other` is in `self`
    pub fn covers(&self, other: TrustedValidatorSet) -> bool {
        self.0 & other.0 == other.0
    }

    /// `self` minus every validator in `other`
    pub fn without(self, other: TrustedValidatorSet) -> TrustedValidatorSet {
        TrustedValidatorSet(self.0 & !other.0)
    }

    /// Positions in the configured trusted validator list, ascending
    pub fn positions(self) -> impl Iterator<Item = usize> {
        (0..Self::MAX).filter(move |position| self.0 & (1u64 << position) != 0)
    }

    /// Trusted validators at `positions` in configured order (each `< TrustedValidatorId::MAX`)
    #[cfg(any(test, feature = "testing"))]
    pub fn at(positions: impl IntoIterator<Item = usize>) -> Self {
        positions
            .into_iter()
            .map(|position| {
                TrustedValidatorId::new(position).unwrap_or_else(|| {
                    panic!("trusted validator {position} past {}", TrustedValidatorId::MAX)
                })
            })
            .collect()
    }
}

impl FromIterator<TrustedValidatorId> for TrustedValidatorSet {
    fn from_iter<I: IntoIterator<Item = TrustedValidatorId>>(validators: I) -> Self {
        let mut set = Self::default();
        for validator in validators {
            set.insert(validator);
        }
        set
    }
}

/// Its chain vs the verified best block, as of its last answered poll
///
/// - `Ahead` = claims higher, its `getblockhash` answer on the verified chain
/// - `Behind` = its claim on the verified chain, below best
/// - `Diverged` = neither (a losing branch: an alarm, never an error, zebra #11133)
/// - `Unknown` = nothing verified yet, or its last poll failed (`Degraded`, `Down`)
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

/// One configured validator as of its last poll (`peers` = telemetry only, never gates serving)
///
/// - `block_at_best` = last `getblockhash` answer, at the best its poll started under
#[derive(Debug, Clone, PartialEq)]
pub struct TrustedValidatorMeta {
    pub address: String,
    pub health: Health,
    pub agreement: Agreement,
    pub observed_at: Option<Instant>,
    pub peers: imbl::Vector<PeerInfo>,
    pub release: Option<NodeRelease>,
    pub streaming: bool,
    pub(crate) info: Option<BlockchainInfo>,
    pub(crate) block_at_best: Option<BlockRef>,
}

impl TrustedValidatorMeta {
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
            block_at_best: None,
        }
    }

    /// Its last poll answered (`Live`, `CatchingUp`): its claim + `block_at_best` are current facts
    pub(crate) fn answering(&self) -> bool {
        matches!(self.health, Health::Live | Health::CatchingUp)
    }

    /// Answering (catching up included), its claim = `chain`'s best, or above it with its last
    /// `getblockhash` answer on `chain` (trusted: holding a block = holding every block below it)
    pub(crate) fn holds(&self, chain: &VerifiedChain) -> bool {
        let best = chain.best();
        let ours = self.block_at_best.is_some_and(|held| chain.on_best(held));
        let above = |claim: BlockRef| claim == best || (claim.height > best.height && ours);
        self.answering() && self.tip().is_some_and(above)
    }

    /// Answering, its claim above `chain`'s best, not shown off it (`block_at_best` unread yet =
    /// maybe ahead: no poll since the chain moved)
    pub(crate) fn claims_past(&self, chain: &VerifiedChain) -> bool {
        let best = chain.best();
        let forked = self
            .block_at_best
            .is_some_and(|held| held.height <= best.height && !chain.on_best(held));
        self.answering() && self.tip().is_some_and(|claim| claim.height > best.height) && !forked
    }

    /// In order: claim = best; claims higher, `block_at_best` verified; claim verified, below; else
    pub(crate) fn agreement_with(&self, chain: Option<&VerifiedChain>) -> Agreement {
        let (Some(chain), Some(claim), true) = (chain, self.tip(), self.answering()) else {
            return Agreement::Unknown;
        };
        let (best, verified) = (chain.best(), |at: BlockRef| chain.on_best(at));
        if claim == best {
            Agreement::Agreed
        } else if claim.height > best.height && self.block_at_best.is_some_and(verified) {
            Agreement::Ahead
        } else if claim.height < best.height && verified(claim) {
            Agreement::Behind
        } else {
            Agreement::Diverged
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
        Some(info.tip)
    }

    /// Blocks its tip trails its own clock estimate by (an eclipsed or stalled node's tell)
    pub fn stale_blocks(&self) -> Option<u32> {
        let (tip, info) = (self.tip()?, self.info.as_ref()?);
        Some(u32::from(info.estimated_height).saturating_sub(u32::from(tip.height)))
    }
}
