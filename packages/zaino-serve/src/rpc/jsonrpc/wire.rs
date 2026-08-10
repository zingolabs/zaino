//! The served JSON-RPC schema.
//!
//! Zaino's JSON-RPC responses are a *wire contract*: zcashd's exact field
//! names, its hex encodings, its byte orders. That contract belongs to the
//! serving adapter, not to the business layer — `zaino-state` returns domain
//! types, and this module is where they become JSON.
//!
//! # Direction and naming
//!
//! Response types go business → wire, which is infallible: a domain value is
//! already valid, so rendering it cannot fail. Each carries a `from_domain`
//! constructor.
//!
//! Request types go the other way, wire → business, and that direction *is* the
//! external-input validation step: the bytes arrived from a client. Each carries
//! an `into_domain` returning `Result`, and the error enumerates the ways a
//! client can be wrong.
//!
//! This deviates from CLAUDE.md's `to_wire` / `try_from_wire` convention, which
//! puts the methods on the business type. That cannot apply here: the business
//! types live in `zaino-primitives` and `zaino-address`, neither of which may
//! depend on serde — which is the point of those crates. So the methods live on
//! the wire type instead, still named rather than `From` impls, so direction and
//! boundary stay readable at the call site.
//!
//! # What is not here
//!
//! Where Zebra already defines the served shape and serializes it correctly, we
//! reuse Zebra's type rather than reimplementing its serde. Only the
//! zcashd-only methods — the ones Zebra has no type for — need a wire type in
//! this module.
//!
//! # Provenance
//!
//! These types were `zaino-fetch`'s response types, which served two roles at
//! once: deserializing validator replies *and* serializing Zaino's own. The
//! first role is now `zaino-source-zebra-rpc`'s, which parses into
//! `zaino-primitives`. Only the second role remains, so the inbound machinery —
//! the per-method error enums, the `ResponseToError` impls, the RPC-error-code
//! mappings — did not come across. `Deserialize` did, where a live test or a
//! client needs to read a response back.

pub mod address;
pub mod address_deltas;
pub mod address_queries;
pub mod block_deltas;
pub mod block_header;
pub mod block_subsidy;
pub mod blockchain_info;
pub mod chain_tips;
pub mod common;
pub mod hashes;
pub mod mining_info;
pub mod misc;
pub mod node_info;
pub mod peer_info;
pub mod subtrees;
pub mod treestate;

/// Renders a 32-byte identifier as hex in RPC display order.
///
/// Block hashes and transaction IDs are byte-reversed for display; tree roots
/// and nonces are not. This is only for the former — reach for plain
/// `hex::encode` for the latter.
pub(crate) fn display_hex(mut bytes: [u8; 32]) -> String {
    bytes.reverse();
    hex::encode(bytes)
}

/// Converts integer zatoshis to the ZEC-denominated float this interface uses.
///
/// Lossy by construction: the interface specifies a JSON number in ZEC, so the
/// conversion happens at the wire boundary and nowhere earlier. Domain types
/// carry integer zatoshis precisely.
pub(crate) fn zats_to_zec(amount: zaino_primitives::types::Zatoshis) -> f64 {
    u64::from(amount) as f64 / 1e8
}

#[cfg(test)]
/// Verifies that a type survives a serialize/deserialize round trip unchanged.
pub(crate) fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + std::fmt::Debug + PartialEq,
{
    let encoded = serde_json::to_string(value).unwrap();
    let decoded: T = serde_json::from_str(&encoded).unwrap();
    assert_eq!(&decoded, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hex in display order is byte-reversed exactly once. Reversing twice, or
    /// not at all, produces a valid-looking hash that names a different block —
    /// so this is pinned against Zebra's own display-order parser.
    #[test]
    fn display_hex_reverses_exactly_once() {
        use hex::FromHex as _;

        // Asymmetric under reversal, so a missing or doubled reverse shows up.
        let bytes: [u8; 32] = core::array::from_fn(|i| i as u8);

        let rendered = display_hex(bytes);
        let parsed = zebra_chain::block::Hash::from_hex(&rendered).expect("valid hash hex");
        assert_eq!(parsed.0, bytes, "one reversal too many or too few");
    }

    /// Amounts cross the port as exact zatoshis and this interface wants ZEC.
    /// The conversion must be exact for every value the protocol allows,
    /// including a single dust zatoshi at the bottom of the range.
    #[test]
    fn zats_to_zec_converts_at_the_wire_boundary() {
        use zaino_primitives::types::Zatoshis;

        assert_eq!(zats_to_zec(Zatoshis::ZERO), 0.0);
        assert_eq!(zats_to_zec(Zatoshis::new(1).unwrap()), 0.000_000_01);
        assert_eq!(zats_to_zec(Zatoshis::new(50_000_000).unwrap()), 0.5);
        assert_eq!(zats_to_zec(Zatoshis::new(100_000_000).unwrap()), 1.0);
    }
}
