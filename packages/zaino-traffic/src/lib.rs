//! Every request to trusted validators and peers, one scheduler (`docs/architecture.md` §9)
//!
//! - [`TrafficBalancer`]: asks by class, each answer naming its sender ([`Answered::ticket`])
//! - [`TrafficDriver`]: one task (the core's clock, every trusted member's poll)
//! - Core: pure members + lanes + one hedge / failover / blame policy, invariants T1–T10

mod balancer;
mod class;
mod core;
mod member;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use balancer::{
    Answered, HeaderAsk, Observation, PeerTransport, Peers, TrafficBalancer, TrafficDriver,
    TrustedValidator, Unanswered, Urgency,
};
pub use core::{Push, Ticket};
pub use member::{Health, Limits, MemberId, MemberRow, MemberTable, TrustedValidatorId};

#[cfg(test)]
mod tests;
