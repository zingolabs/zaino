//! Domain results of the two address-validation RPCs.
//!
//! Both types encode validity in the enum discriminant rather than in an
//! `is_valid` field: an invalid address has no address data, so representing
//! the two states as one struct with optional fields admits combinations that
//! cannot occur.

/// Deprecation notice for the `z_validateaddress` endpoint.
///
/// Emitted at runtime via `tracing::warn!` by [`super::z_validate_address`] and
/// referenced from doc comments on the RPC methods that expose it.
pub const DEPRECATION_NOTICE: &str = "z_validateaddress is deprecated: delegating address validation to a non-client actor encourages information leakage. This service is only offered for bugwards compatibility with zcashd, and WILL BE REMOVED.";

/// The result of the `validateaddress` RPC.
///
/// zcashd's `validateaddress` recognises transparent addresses only; a
/// well-formed shielded address is reported invalid rather than described.
/// Zaino matches that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidatedAddress {
    /// The address did not parse, was for another network, or was shielded.
    Invalid,

    /// A transparent address.
    Transparent {
        /// The address as supplied by the caller.
        address: String,
        /// `true` for P2SH, `false` for P2PKH.
        is_script: bool,
    },
}

/// The result of the deprecated `z_validateaddress` RPC.
///
/// # Sprout
///
/// There is no Sprout variant. Zaino does not classify Sprout addresses — the
/// previous implementation fell through to "invalid" for them, and Zaino does
/// not serve Sprout data anywhere else either. Modelling a variant that cannot
/// be constructed would claim a capability Zaino does not have.
///
/// # Sapling key material
///
/// The Sapling variant carries the diversifier and pk_d as fixed-size byte
/// arrays, both mandatory. A valid Sapling address always has both, so the
/// previous representation — two independent `Option<String>` fields plus a
/// runtime cross-field check rejecting the half-populated cases — encoded an
/// invariant the type can state outright. Hex encoding happens at the wire
/// boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZValidatedAddress {
    /// The address did not parse, was for another network, or is a kind Zaino
    /// does not classify.
    Invalid,

    /// Transparent pay-to-public-key-hash.
    P2pkh {
        /// The address as supplied by the caller.
        address: String,
    },

    /// Transparent pay-to-script-hash.
    P2sh {
        /// The address as supplied by the caller.
        address: String,
    },

    /// Sapling shielded payment address.
    Sapling {
        /// The address, re-encoded for the queried network.
        address: String,
        /// The diversifier `d`.
        diversifier: [u8; 11],
        /// The diversified transmission key `pk_d`, in zcashd's big-endian
        /// byte order (see [`super::sapling_key_bytes`]).
        diversified_transmission_key: [u8; 32],
    },

    /// Unified address. zcashd reports no components for these, and neither
    /// does Zaino.
    Unified {
        /// The address, re-encoded for the queried network.
        address: String,
    },
}
