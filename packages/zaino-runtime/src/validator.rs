//! The validator as an **observed** component.
//!
//! The validator is the root of the lifecycle DAG (ADR-0014): every other
//! component builds on it, so the runtime confirms it is live before booting
//! dependents. But it is *external* — the runtime cannot spawn or restart it —
//! so [`ValidatorComponent`] implements [`StatusSource`] / [`StatusWatch`] but
//! **not** [`Managed`](zaino_component::Managed). It is booted with
//! [`OrchestraBuilder::boot_observed`](crate::OrchestraBuilder::boot_observed).
//!
//! An observed component's health is driven from the *outside*: the source layer
//! that talks to the validator calls [`report_health`] when it sees the
//! connection drop or recover. That is the defining difference from an owned
//! component (e.g. a server), whose health comes from its own task.
//!
//! [`report_health`]: ValidatorComponent::report_health

use core::future::Future;

use tokio::sync::watch;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusSource, StatusWatch,
};

const VALIDATOR: ComponentName = ComponentName("validator");

/// A reachability check against the validator — the minimal thing the runtime
/// needs to gate bringup. The real implementor is a source client; a test uses
/// a stub.
pub trait ValidatorProbe {
    /// Whether the validator is reachable right now.
    fn reachable(&self) -> impl Future<Output = bool> + Send;
}

/// The validator could not be reached at bringup; dependents must not boot.
#[derive(Debug, thiserror::Error)]
#[error("validator is unreachable")]
pub struct ValidatorUnreachable;

/// The validator, presented to the runtime as an observed component.
#[derive(Clone)]
pub struct ValidatorComponent {
    status: watch::Sender<ComponentStatus>,
}

impl ValidatorComponent {
    /// Confirm the validator is reachable, and present it as a `Ready` component.
    ///
    /// Errors if the initial probe fails: the runtime gates dependents on the
    /// validator, so a missing validator is a boot failure, not a degraded start.
    pub async fn connect<P: ValidatorProbe>(probe: &P) -> Result<Self, ValidatorUnreachable> {
        if !probe.reachable().await {
            return Err(ValidatorUnreachable);
        }
        let (status, _) = watch::channel(ComponentStatus::new(
            VALIDATOR,
            Lifecycle::Ready,
            Health::Healthy,
        ));
        Ok(Self { status })
    }

    /// Report an observed change in the validator's condition — what the source
    /// layer calls when it detects the validator drop (`Critical`) or recover
    /// (`Healthy`). This is how an observed component's health is driven: from
    /// the outside, not from a task the runtime owns.
    pub fn report_health(&self, health: Health) {
        self.status.send_modify(|s| s.health = health);
    }
}

impl StatusSource for ValidatorComponent {
    fn status(&self) -> ComponentStatus {
        *self.status.borrow()
    }
}

impl StatusWatch for ValidatorComponent {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}
