//! A component's lifecycle and the transitions it permits.
//!
//! ```text
//! Offline ──▶ Spawning ──▶ Syncing ⇄ Ready
//!    ▲            │           │        │
//!    │            └───────────┴────────┤
//!    └────────── Closing ◀─────────────┘
//! ```
//!
//! A restart is a full lap — `Closing → Offline → Spawning` — never a jump
//! straight back to running. Self-transitions are legal, as idempotent
//! re-reports. [`Lifecycle::try_advance_to`] is the only mover.

/// Where a component is in its managed progression — a *phase*.
///
/// Moved only by management ([`Managed`](crate::Managed)): spawn advances it,
/// stop retires it. One of the two axes of a
/// [`ComponentStatus`](crate::ComponentStatus); see the crate documentation for
/// how it relates to [`Health`](crate::Health).
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display)]
pub enum Lifecycle {
    /// Not running — the rest state a component starts in and returns to after
    /// it has finished closing. The only phase a fresh spawn may begin from.
    Offline,
    /// Starting up; not yet doing its work.
    Spawning,
    /// Running, but still catching up to where it can serve.
    Syncing,
    /// Running and able to serve.
    Ready,
    /// Shutting down; on its way to [`Offline`](Self::Offline).
    Closing,
}

impl Lifecycle {
    /// Advance to `next` if the module's state machine allows it, returning the
    /// new phase; otherwise return the rejected [`IllegalTransition`].
    ///
    /// The lifecycle owns its own transitions: nothing else, not even the
    /// enclosing [`ComponentStatus`](crate::ComponentStatus), performs them.
    pub fn try_advance_to(self, next: Lifecycle) -> Result<Lifecycle, IllegalTransition> {
        use Lifecycle::{Closing, Offline, Ready, Spawning, Syncing};
        let legal = matches!(
            (self, next),
            (Offline, Offline | Spawning)
                | (Spawning, Spawning | Syncing | Ready | Closing)
                | (Syncing, Syncing | Ready | Closing)
                | (Ready, Ready | Syncing | Closing)
                | (Closing, Closing | Offline)
        );
        if legal {
            Ok(next)
        } else {
            Err(IllegalTransition {
                from: self,
                to: next,
            })
        }
    }
}

/// A lifecycle transition the state machine forbids.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("illegal lifecycle transition: {from} -> {to}")]
pub struct IllegalTransition {
    /// The phase the component was in.
    pub from: Lifecycle,
    /// The phase the transition tried to reach.
    pub to: Lifecycle,
}
