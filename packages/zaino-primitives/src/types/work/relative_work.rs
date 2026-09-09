//! The relative quantity: work accumulated since an anchor.

use core::fmt;

/// Work accumulated since an anchor: the fold of block works along a branch
/// that starts at some cumulative-work anchor rather than at genesis.
///
/// The finalised state's tip carries absolute cumulative work
/// ([`ChainWork`](super::ChainWork)); a non-finalised branch carries the work
/// accumulated *since* that anchor. Answering "absolute work at the branch
/// tip" combines the two, through the `extend` relation in the `arithmetic`
/// module.
///
/// Zero is admissible — a branch whose tip *is* the anchor has accumulated
/// nothing — and that is what separates this quantity from [`ChainWork`],
/// which is strictly positive. Machine representability is the only invariant
/// here, so the honest constructor, [`new`](Self::new), is infallible, and
/// zero is a real value of the quantity, never an absence sentinel.
///
/// `zaino-chain-head` models this quantity today as its crate-local
/// `ChainHeadWork` (mirroring zebra's `Work` vs `PartialCumulativeWork`
/// split); collapsing that duplicate onto this primitive is a planned
/// follow-up.
///
/// [`ChainWork`]: super::ChainWork
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RelativeWork(u128);

impl RelativeWork {
    /// The branch that has accumulated nothing: its tip is its anchor.
    pub const ZERO: Self = Self(0);

    /// Create a relative work value.
    ///
    /// Infallible: machine representability is the only invariant this
    /// quantity carries. Zero is a real value — a branch whose tip is its
    /// anchor — so there is no bound to check and no door to refuse at.
    pub const fn new(value: u128) -> Self {
        Self(value)
    }
}

impl From<RelativeWork> for u128 {
    fn from(work: RelativeWork) -> Self {
        work.0
    }
}

impl fmt::Debug for RelativeWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RelativeWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for RelativeWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero is a value of the quantity, reachable through both doors.
    #[test]
    fn zero_is_a_value_not_an_absence() {
        assert_eq!(u128::from(RelativeWork::ZERO), 0);
        assert_eq!(RelativeWork::new(0), RelativeWork::ZERO);
    }

    #[test]
    fn new_round_trips() {
        assert_eq!(u128::from(RelativeWork::new(0x2a2a)), 0x2a2a);
    }
}
