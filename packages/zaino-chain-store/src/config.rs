//! How a chain store is configured.

use core::time::Duration;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};

/// Deployment settings every chain store is configured from.
///
/// The backend-neutral half. A store will need knobs this cannot anticipate —
/// ZainoDB needs an LMDB map size and an activation schedule, neither of which
/// a domain crate can name without taking a dependency the split exists to
/// avoid — so an implementation pairs this with its own type rather than
/// extending this one. `zaino_chain_store_zainodb::ZainoDbConfig` is that
/// pairing, and it holds *only* what this does not.
///
/// Deliberately says nothing about the network. Pool activation heights are
/// what a backend needs a network for, and that is a property of how it builds
/// blocks rather than of what a store is.
///
/// # Fields are private, and the illegal states are unrepresentable
///
/// Where a store lives and whether it holds anything are **one** field, not a
/// path beside a boolean: a store configured both to hold nothing and to hold
/// it somewhere is a contradiction an operator should not be able to express,
/// and it is not one a runtime check catches well — the two orderings disagree
/// about which wins.
///
/// Zero is meaningless for three of the four remaining knobs, so it is made
/// unrepresentable rather than checked at startup:
///
/// - `target_schema_major` of zero names no schema, so a store could not decide
///   what to open.
/// - `retry_backoff` of zero retries a failing validator in a tight loop, which
///   damages the node being polled rather than this one.
/// - `max_consecutive_failures` of zero and of one are the same thing — the
///   count is compared after it is incremented — so zero buys no behaviour that
///   one does not.
///
/// `background_build_threshold` keeps its zero, because zero is meaningful
/// there: it means every build runs in the background. That is a real, if
/// rarely wanted, configuration, and following the `NonZero` pattern for
/// uniformity would have removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStoreConfig {
    path: Option<PathBuf>,
    target_schema_major: NonZeroU32,
    background_build_threshold: u32,
    retry_backoff_ms: NonZeroU64,
    max_consecutive_failures: NonZeroU32,
}

/// A `NonZeroU32` from a literal default below.
fn nz32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("a literal default is not zero")
}

/// A `NonZeroU64` from a literal default below.
fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("a literal default is not zero")
}

impl Default for ChainStoreConfig {
    /// A store that holds nothing and answers by passing reads through.
    ///
    /// Passthrough rather than a path, because there is no path a default could
    /// pick that would be right for a deployment — and a store that holds
    /// nothing is the one state that needs no answer to "where".
    fn default() -> Self {
        Self {
            path: None,
            target_schema_major: nz32(1),
            background_build_threshold: 10,
            retry_backoff_ms: nz64(5_000),
            max_consecutive_failures: nz32(5),
        }
    }
}

impl ChainStoreConfig {
    /// A store persisting at `path`, with production defaults otherwise.
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            ..Self::default()
        }
    }

    /// Where the store lives, or `None` for one that holds nothing.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether this configures a store that holds nothing.
    pub fn is_passthrough_only(&self) -> bool {
        self.path.is_none()
    }

    /// The schema version to bring the store to on open.
    ///
    /// A store on an older schema migrates up to this; one on a newer schema is
    /// refused, because this build does not know what it would be reading.
    pub fn target_schema_major(&self) -> u32 {
        self.target_schema_major.get()
    }

    /// How far behind the target the store may be before it builds in the
    /// background rather than blocking the caller.
    ///
    /// Below this a caller waits and gets a store that is ready, which is what
    /// lets a caller read straight back after asking for a short build. Above
    /// it, waiting would mean an unavailable node for hours, so the store comes
    /// up serving passthrough reads and catches up behind them.
    pub fn background_build_threshold(&self) -> u32 {
        self.background_build_threshold
    }

    /// Delay between build attempts after a failure.
    pub fn retry_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_backoff_ms.get())
    }

    /// Consecutive failures tolerated before the store reports itself
    /// critically failed.
    ///
    /// A budget rather than a timeout: a validator down for an hour and then
    /// back should leave the store healthy, but one that rejects every request
    /// is a condition an operator has to hear about.
    pub fn max_consecutive_failures(&self) -> u32 {
        self.max_consecutive_failures.get()
    }

    /// Set the schema version to target.
    pub fn set_target_schema_major(&mut self, major: NonZeroU32) {
        self.target_schema_major = major;
    }

    /// Set how far behind the target a build may be before it backgrounds.
    pub fn set_background_build_threshold(&mut self, blocks: u32) {
        self.background_build_threshold = blocks;
    }

    /// Set the delay between build attempts after a failure.
    pub fn set_retry_backoff_ms(&mut self, millis: NonZeroU64) {
        self.retry_backoff_ms = millis;
    }

    /// Set how many consecutive failures are tolerated.
    pub fn set_max_consecutive_failures(&mut self, failures: NonZeroU32) {
        self.max_consecutive_failures = failures;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holding nothing and holding it somewhere are mutually exclusive by
    /// construction, not by validation.
    #[test]
    fn a_path_and_passthrough_only_cannot_both_be_configured() {
        assert!(ChainStoreConfig::default().is_passthrough_only());
        assert!(ChainStoreConfig::default().path().is_none());

        let persistent = ChainStoreConfig::at_path("/tmp/store");
        assert!(!persistent.is_passthrough_only());
        assert_eq!(persistent.path(), Some(Path::new("/tmp/store")));
    }

    /// The defaults are the values the only implementation runs on.
    ///
    /// Pinned because this type was declared before anything consumed it, and a
    /// default that drifted from the constant it replaced would change how a
    /// store builds without anything saying so.
    #[test]
    fn the_defaults_match_what_the_backend_ran_on() {
        let config = ChainStoreConfig::default();
        assert_eq!(config.target_schema_major(), 1);
        assert_eq!(config.background_build_threshold(), 10);
        assert_eq!(config.retry_backoff(), Duration::from_secs(5));
        assert_eq!(config.max_consecutive_failures(), 5);
    }
}
