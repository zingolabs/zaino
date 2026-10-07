//! Every request to trusted validators and peers, one scheduler (`docs/design/traffic-balancer.md`)
//!
//! - [`TrafficBalancer`]: asks by class, each answer naming its sender ([`Answered::ticket`])
//! - [`TrafficDriver`]: one task (the core's clock, every trusted member's poll, peers joining)
//! - Core: pure members + classes + one hedge / retry / blame policy, invariants T1–T10

mod balancer;
mod class;
mod core;
mod limits;
mod member;

pub use balancer::{
    Answered, HeaderAsk, Membership, Observation, PeerTransport, TrafficBalancer, TrafficDriver,
    Trusted, Unanswered, Urgency,
};
pub use core::{Push, Ticket};
pub use member::{Health, Limits, MemberId, MemberRow, MemberTable, PeerId, ValidatorId};

/// Panic message of `run`, `None` = no panic (fire drills)
#[cfg(test)]
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}

#[cfg(test)]
mod tests;
