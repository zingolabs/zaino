//! Zebra validator source.
//!
//! Reaches a Zebra validator over its JSON-RPC interface, which answers every
//! query — blocks, the mempool, the passthrough RPCs, and the derived queries
//! the validator computes — at the cost of a request/response round-trip per
//! call.
//!
//! [`ZebraValidator`] wraps the JSON-RPC adapter and adds a synthesised tip
//! subscription: Zebra pushes no tip stream, so the source polls for it (see
//! [`ZebraValidator::with_tip_polling`]).

mod routing;

pub use routing::ZebraValidator;
