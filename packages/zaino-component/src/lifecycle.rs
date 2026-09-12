//! A component's lifecycle and the transitions it permits.

/// Where a component is in its managed progression — a *phase*.
///
/// Moved only by management ([`Managed`](crate::Managed)): spawn advances it,
/// stop retires it. Distinct from [`Health`](crate::Health), which is a
/// condition the component reaches on its own. A component being `Ready` says
/// nothing about whether it is `Healthy`, and vice versa.
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
    /// Advance to `next` if the state machine allows it, returning the new phase;
    /// otherwise return the rejected [`IllegalTransition`].
    ///
    /// The lifecycle owns its own transitions — nothing else, not even the
    /// enclosing [`ComponentStatus`](crate::ComponentStatus), validates or
    /// performs them. The cycle: a component starts `Offline`, is `Spawning`ed,
    /// may `Syncing` and/or become `Ready` (and drop back to `Syncing` if it
    /// falls behind), then `Closing` on shutdown, ending back at `Offline`. A
    /// restart is a full lap — `Closing → Offline → Spawning` — never a jump
    /// straight back to running. Self-transitions are allowed, as idempotent
    /// re-reports.
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
