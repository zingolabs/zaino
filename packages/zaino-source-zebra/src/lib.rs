//! Composite Zebra source.
//!
//! Reaching a Zebra validator involves two transports with different reach.
//! Reading its state database directly is far faster but answers only what the
//! finalized state holds; the JSON-RPC interface answers everything but pays
//! request/response cost for each call.
//!
//! The previous implementation expressed this as an enum whose every method
//! matched on the arm, which conflated two separate questions: *can* this
//! transport answer, and *should* it when both can. Here the two are separate
//! mechanisms:
//!
//! - **Capability is structural.** `ZebraReadStateAdapter` simply does not
//!   implement the traits it cannot serve — the node passthroughs, the mempool,
//!   the derived delta queries. There is no arm to get wrong, and a
//!   deployment's transport mix cannot silently change what a query means.
//! - **Preference is this routing table.** Each trait below delegates: to the
//!   state service where it can answer, to JSON-RPC otherwise, and — in the two
//!   cases where the fast path is *semantically* incomplete rather than merely
//!   absent — to the state service first and JSON-RPC after.
//!
//! The JSON-RPC adapter is not optional. Every deployment has one, because the
//! mempool and the passthrough RPCs are reachable no other way. The state
//! adapter is the accelerator layered over it, which is why it is the `Option`.

mod fallback;
mod routing;

pub use routing::ZebraValidator;
