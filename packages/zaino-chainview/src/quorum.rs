//! Majority of the **configured** set.

use std::num::NonZeroUsize;

use zaino_primitives::types::BlockRef;

use crate::endpoints::EndpointSet;
use crate::error::BelowQuorum;

/// `⌊N/2⌋ + 1` over the configured endpoints.
///
/// Configured, not responding: majority-of-responding is trivially subvertible by DoSing the
/// honest nodes (`docs/design/chainview.md` §4). N=1 gives threshold 1 — quorum trivially met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quorum {
    configured: NonZeroUsize,
    threshold: NonZeroUsize,
}

impl Quorum {
    pub(crate) fn over(configured: NonZeroUsize) -> Self {
        let threshold = configured.get() / 2 + 1;
        Self {
            configured,
            threshold: NonZeroUsize::new(threshold).expect("n/2 + 1 >= 1 for any n"),
        }
    }

    pub fn configured(&self) -> usize {
        self.configured.get()
    }

    pub fn threshold(&self) -> usize {
        self.threshold.get()
    }

    pub(crate) fn met_by(&self, endpoints: EndpointSet) -> bool {
        endpoints.count() >= self.threshold()
    }

    pub(crate) fn shortfall(&self, endpoints: EndpointSet) -> BelowQuorum {
        BelowQuorum {
            agreeing: endpoints.count(),
            threshold: self.threshold(),
            configured: self.configured(),
        }
    }
}

/// A tip ≥threshold endpoints agree on, **by hash**.
///
/// Never the maximum height, or one node claiming height 999,999 moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuorumTip {
    pub block: BlockRef,
    pub agreed_by: EndpointSet,
}
