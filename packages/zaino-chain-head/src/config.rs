//! How the ChainHead runtime is configured.

use std::time::Duration;

/// How far below the tip ChainHead retains blocks, and how hard it polls.
///
/// [`Default`] gives the consensus reorg bound, which is what production wants.
/// `max_depth` is still a field rather than a constant because the window is a
/// deployment fact: tests need a tractable depth to exercise a *moving* seam
/// against short chains, and they set it here rather than through a
/// compile-time feature this crate would otherwise have to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChainHeadConfig {
    /// Blocks retained below the canonical tip.
    ///
    /// The retention floor is `best_tip.height - max_depth`. This also bounds
    /// every ancestry walk: neither reorg handling nor competing-branch
    /// resolution should recurse further back than the window it maintains.
    pub max_depth: u32,

    /// How often to re-read the source when nothing wakes ChainHead sooner.
    ///
    /// Correctness never depends on this: a wake is a latency hint, and
    /// ChainHead re-reads the source on every wake regardless.
    pub poll_interval: Duration,

    /// Delay before the first retry after a source failure. Doubles on each
    /// consecutive failure up to `max_backoff`.
    pub initial_backoff: Duration,

    /// Ceiling on the doubling backoff.
    pub max_backoff: Duration,

    /// Consecutive source failures tolerated before ChainHead reports itself
    /// critically failed.
    ///
    /// A validator that is briefly unreachable should not take ChainHead down;
    /// one that stays unreachable should not be reported as healthy.
    pub max_consecutive_failures: u32,
}

impl Default for ChainHeadConfig {
    fn default() -> Self {
        Self {
            max_depth: zaino_consensus::MAX_NONFINALISED_DEPTH,
            poll_interval: Duration::from_secs(1),
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            max_consecutive_failures: 10,
        }
    }
}

impl ChainHeadConfig {
    /// The default configuration, retaining `max_depth` blocks instead of the
    /// consensus reorg bound.
    ///
    /// For callers that need a shallower window than production — a test
    /// exercising eviction against a chain far shorter than
    /// [`MAX_NONFINALISED_DEPTH`](zaino_consensus::MAX_NONFINALISED_DEPTH),
    /// where the real depth would keep every block it ever saw and the seam
    /// would never move.
    pub fn with_max_depth(max_depth: u32) -> Self {
        Self {
            max_depth,
            ..Self::default()
        }
    }
}
