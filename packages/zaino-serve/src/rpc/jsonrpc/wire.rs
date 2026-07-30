//! The served JSON-RPC schema.
//!
//! Zaino's JSON-RPC responses are a *wire contract*: zcashd's exact field
//! names, its hex encodings, its byte orders. That contract belongs to the
//! serving adapter, not to the business layer — `zaino-state` returns domain
//! types, and this module is where they become JSON.
//!
//! # Direction and naming
//!
//! Everything here goes business → wire, which is infallible: a domain value is
//! already valid, so rendering it cannot fail. Each wire type therefore carries
//! a `from_domain` constructor.
//!
//! This deviates from CLAUDE.md's `to_wire` convention, which puts the method on
//! the business type. It cannot apply here: the business types live in
//! `zaino-primitives` and `zaino-address`, neither of which may depend on serde
//! — that is the whole point of those crates. So the method lives on the wire
//! type instead, still named rather than a `From` impl, so that direction and
//! boundary stay readable at the call site.
//!
//! # What is not here
//!
//! Where Zebra already defines the served shape and serializes it correctly, we
//! reuse Zebra's type rather than reimplementing its serde. Only the
//! zcashd-only methods — the ones Zebra has no type for — need a wire struct in
//! this module.

pub mod address;
