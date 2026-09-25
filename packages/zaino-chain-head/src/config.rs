//! How the ChainHead runtime is configured.

use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

/// How far below the tip ChainHead retains blocks, and how often it polls.
///
/// [`Default`] gives the consensus reorg bound, which is what production wants.
/// `max_depth` is still configurable because the window is a deployment fact:
/// tests need a tractable depth to exercise a *moving* seam against short
/// chains, and they set it here rather than through a compile-time feature this
/// crate would otherwise have to carry.
///
/// # No retry knobs
///
/// There is no backoff here because the chain head does not retry: it binds
/// the canonical `zaino-source` ports, which are answered by a
/// `ValidatorClient` that has already retried transient failures under its
/// own `RetryPolicy`. A failure the chain head sees is that ladder spent, and
/// its answer is to report it and try again on the next poll — one place
/// decides how hard a validator is retried, and it is not this one.
///
/// # Both knobs are `NonZero`, and that is not uniformity for its own sake
///
/// Zero is meaningless for both, so it is made unrepresentable rather than
/// checked at startup — the failures it produces are silent or late:
///
/// - `max_depth` of zero retains only the tip, so the chain head could not
///   observe the reorgs it exists to hold.
/// - `poll_interval` of zero turns the writer loop into a spin against the
///   validator. It does not panic — the loop sleeps rather than ticking — which
///   is worse than panicking, because the damage lands on the node being polled.
///
/// Contrast a knob where zero *is* meaningful, which would stay plain: there is
/// none here. If one is added, it keeps its zero rather than taking `NonZero`
/// for consistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHeadConfig {
    max_depth: NonZeroU32,
    poll_interval_ms: NonZeroU64,
}

/// The consensus reorg bound, as a `NonZeroU32`.
///
/// `expect` rather than a fallible path: this reads a compile-time constant
/// that is a reorg bound, and a reorg bound of zero would mean the chain never
/// reorganises. If that ever became true, the whole crate would be pointless.
fn default_max_depth() -> NonZeroU32 {
    NonZeroU32::new(zaino_consensus::MAX_NONFINALISED_DEPTH)
        .expect("the consensus reorg bound is not zero")
}

/// Milliseconds as a `NonZeroU64`, for the literal defaults below.
///
/// Private and only ever called with a literal, so the `expect` asserts
/// something the reader can check at the call site.
fn ms(millis: u64) -> NonZeroU64 {
    NonZeroU64::new(millis).expect("a literal default interval is not zero")
}

impl Default for ChainHeadConfig {
    fn default() -> Self {
        Self {
            max_depth: default_max_depth(),
            poll_interval_ms: ms(1_000),
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
    pub fn with_max_depth(max_depth: NonZeroU32) -> Self {
        Self {
            max_depth,
            ..Self::default()
        }
    }

    /// Blocks retained below the canonical tip.
    ///
    /// The retention floor is `best_tip.height - max_depth`. This also bounds
    /// every ancestry walk: neither reorg handling nor competing-branch
    /// resolution should recurse further back than the window it maintains.
    pub fn max_depth(&self) -> u32 {
        self.max_depth.get()
    }

    /// How often to re-read the source when nothing wakes ChainHead sooner.
    ///
    /// Correctness never depends on this: a wake is a latency hint, and
    /// ChainHead re-reads the source on every wake regardless.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.get())
    }

    /// Set the poll interval, in milliseconds.
    pub fn set_poll_interval_ms(&mut self, millis: NonZeroU64) {
        self.poll_interval_ms = millis;
    }
}
