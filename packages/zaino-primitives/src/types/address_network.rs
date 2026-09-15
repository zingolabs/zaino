//! The Zcash network an address encodes for.

/// The network an encoded Zcash address belongs to.
///
/// An address string commits to a network in its encoding — the same key
/// material produces a different string on mainnet than on testnet — so the
/// network is a fact *about the address*, read off it at parse time, not a
/// separate configuration a consumer supplies.
///
/// This is Zaino's own vocabulary: the address parser inside
/// [`TransparentAddress`](crate::types::TransparentAddress) maps the underlying
/// protocol library's network tag onto this enum so that nothing from that
/// library reaches Zaino's API surface.
///
/// Transparent testnet and regtest addresses share one base58 encoding, so a
/// transparent address parsed from a string is only ever [`Mainnet`] or
/// [`Testnet`]; [`Regtest`] is carried for completeness of the mapping.
///
/// [`Mainnet`]: AddressNetwork::Mainnet
/// [`Testnet`]: AddressNetwork::Testnet
/// [`Regtest`]: AddressNetwork::Regtest
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressNetwork {
    /// Zcash mainnet.
    Mainnet,
    /// Zcash testnet.
    Testnet,
    /// A private regression-testing network.
    Regtest,
}
