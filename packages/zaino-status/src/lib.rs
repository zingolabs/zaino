//! How a Zaino component reports whether it is working.
//!
//! [`StatusType`] is the vocabulary — the states a component can be in and how
//! two of them combine. [`Status`] is how a component reports one, and
//! [`Liveness`]/[`Readiness`] are the two questions an operator or orchestrator
//! actually asks, derived from it by blanket impl.
//!
//! # Why this is its own crate
//!
//! Status is the one thing *every* subsystem has, including the ones whose
//! whole purpose is to depend on as little as possible. Keeping this vocabulary
//! in a general-purpose crate meant reporting a status cost a dependency on
//! that crate's entire graph — the validator config, the logging stack, TLS,
//! `zebra-chain`. A subsystem should be able to say "I am syncing" without any
//! of that.
//!
//! Deps stay at `tracing` + (optional) the `metrics` facade — vocabulary, not
//! machinery.

/// Prometheus metric names emitted by this crate; the single source of truth
/// shared with `zainod`'s `describe_*` registrations, which carry the
/// descriptions.
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    /// Each component's [`StatusType`](crate::StatusType) discriminant, labelled
    /// by [`STATUS_COMPONENT`].
    ///
    /// - Emitted from `NamedAtomicStatus::store`, which every change passes
    ///   through → series set = the component set, no registration to forget
    pub const STATUS: &str = "zaino.status";

    /// Component a [`STATUS`] sample describes, from `NamedAtomicStatus::name`.
    pub const STATUS_COMPONENT: &str = "component";

    /// [`StatusType`](crate::StatusType) names, indexed by discriminant.
    ///
    /// - `zainod` renders it into help text, so no dashboard retypes (and drifts)
    pub const STATUS_VALUES: [&str; 8] = [
        "spawning",
        "syncing",
        "ready",
        "busy",
        "closing",
        "offline",
        "recoverable-error",
        "critical-error",
    ];
}

pub mod probing;
pub mod status;

pub use probing::{Liveness, Readiness, VitalsProbe};
pub use status::{NamedAtomicStatus, Status, StatusType};

#[cfg(all(test, feature = "prometheus"))]
mod metric_tests {
    use crate::{metric_names::STATUS_VALUES, StatusType};

    /// - `zainod`'s legend test renders from this same array, so it cannot catch
    ///   a legend disagreeing with the enum; this ties the two together
    /// - Without it a reorder or append silently makes every dashboard name the
    ///   wrong state
    #[test]
    fn status_discriminants_match_their_legend_positions() {
        for (status, expected) in [
            (StatusType::Spawning, "spawning"),
            (StatusType::Syncing, "syncing"),
            (StatusType::Ready, "ready"),
            (StatusType::Busy, "busy"),
            (StatusType::Closing, "closing"),
            (StatusType::Offline, "offline"),
            (StatusType::RecoverableError, "recoverable-error"),
            (StatusType::CriticalError, "critical-error"),
        ] {
            let discriminant = status as usize;
            assert_eq!(
                STATUS_VALUES.get(discriminant),
                Some(&expected),
                "{status:?} publishes as {discriminant}, but the legend calls that \
                 position {:?}",
                STATUS_VALUES.get(discriminant),
            );
        }
        assert_eq!(
            STATUS_VALUES.len(),
            8,
            "a StatusType variant was added or removed without updating STATUS_VALUES"
        );
    }
}
