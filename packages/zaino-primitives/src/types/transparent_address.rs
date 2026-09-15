//! Transparent Zcash address.

use zcash_address::{ConversionError, TryFromAddress, ZcashAddress};
use zcash_protocol::consensus::NetworkType;

use crate::types::{AddressNetwork, ScriptType};

/// A transparent Zcash address.
///
/// Holds the canonical encoded string, and only ever a string that is a
/// well-formed transparent (P2PKH or P2SH) address. The constructor
/// [`try_new`](TransparentAddress::try_new) is the one place the format is
/// checked, so every consumer inherits a validated address for free: there is
/// no unchecked door.
///
/// # What validates
///
/// Construction parses the string with the `zcash_address` protocol library and
/// accepts *only* the two transparent kinds. Shielded (Sapling), unified,
/// Sprout, and TEX addresses decode successfully but are rejected as
/// [`TransparentAddressError::NotTransparent`]; anything that does not decode as
/// a Zcash address at all is rejected as
/// [`TransparentAddressError::Undecodable`] with the parser's reason preserved.
///
/// The address library is an implementation detail: none of its types appear in
/// this type's API. What the address *is* — its network and script form — is
/// re-expressed in Zaino's own vocabulary ([`AddressNetwork`], [`ScriptType`]).
///
/// # Network-blindness
///
/// A transparent address is accepted regardless of which network it encodes
/// for; the network is then reported by [`network`](TransparentAddress::network).
/// The primitive states what the address *is*; deciding whether that network is
/// the one a given query should act on is the consumer's policy, not the
/// address's invariant.
///
/// # Stored metadata
///
/// The network and script form are parsed once at construction and stored
/// alongside the string, so [`network`](TransparentAddress::network) and
/// [`script_type`](TransparentAddress::script_type) are field reads rather than
/// a re-parse on every call. The two extra machine words are cheaper than
/// re-running base58 decoding at each accessor, and both are `Copy` so the
/// struct stays cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransparentAddress {
    encoded: String,
    network: AddressNetwork,
    script_type: ScriptType,
}

impl TransparentAddress {
    /// Parses and validates a transparent address string.
    ///
    /// # Errors
    ///
    /// - [`TransparentAddressError::Undecodable`] if the string is not a
    ///   decodable Zcash address.
    /// - [`TransparentAddressError::NotTransparent`] if it decodes as a Zcash
    ///   address of a non-transparent kind (shielded, unified, Sprout, TEX).
    pub fn try_new(address: impl Into<String>) -> Result<Self, TransparentAddressError> {
        let encoded = address.into();
        let parsed = ZcashAddress::try_from_encoded(&encoded)
            .map_err(|e| TransparentAddressError::Undecodable(e.to_string()))?;

        // `convert` dispatches on the parsed address kind without checking the
        // network, so acceptance is network-blind. The only failure it can
        // produce for a successfully decoded address is `Unsupported`, raised by
        // the defaulted non-transparent arms of `TransparentKind` below — hence
        // any error here means "decoded, but not transparent".
        let kind = parsed
            .convert::<TransparentKind>()
            .map_err(|e| TransparentAddressError::NotTransparent(e.to_string()))?;

        Ok(Self {
            encoded,
            network: kind.network,
            script_type: kind.script_type,
        })
    }

    /// The network this address encodes for.
    pub fn network(&self) -> AddressNetwork {
        self.network
    }

    /// The transparent script form this address pays to — [`ScriptType::P2PKH`]
    /// or [`ScriptType::P2SH`].
    pub fn script_type(&self) -> ScriptType {
        self.script_type
    }

    /// The canonical encoded address string.
    pub fn as_str(&self) -> &str {
        &self.encoded
    }
}

impl core::fmt::Display for TransparentAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.encoded)
    }
}

impl From<TransparentAddress> for String {
    fn from(a: TransparentAddress) -> Self {
        a.encoded
    }
}

/// The transparent metadata extracted while parsing an address.
///
/// A private carrier for the `zcash_address` conversion: implementing
/// [`TryFromAddress`] lets `convert` hand back exactly the two facts Zaino
/// keeps — network and script form — for the transparent kinds, while the
/// defaulted arms reject every other kind. It never crosses this module's
/// boundary, so the address library stays contained here.
struct TransparentKind {
    network: AddressNetwork,
    script_type: ScriptType,
}

impl TryFromAddress for TransparentKind {
    // No transparent arm produces a user error, so the user-error channel is
    // uninhabited.
    type Error = core::convert::Infallible;

    fn try_from_transparent_p2pkh(
        net: NetworkType,
        _data: [u8; 20],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from(net),
            script_type: ScriptType::P2PKH,
        })
    }

    fn try_from_transparent_p2sh(
        net: NetworkType,
        _data: [u8; 20],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            network: network_from(net),
            script_type: ScriptType::P2SH,
        })
    }
}

/// Maps the protocol library's network tag onto Zaino's [`AddressNetwork`].
///
/// The match is exhaustive over `NetworkType`'s three variants, so a new kind
/// of network in the library would surface here as a compile error rather than
/// a silent mis-mapping.
fn network_from(net: NetworkType) -> AddressNetwork {
    match net {
        NetworkType::Main => AddressNetwork::Mainnet,
        NetworkType::Test => AddressNetwork::Testnet,
        NetworkType::Regtest => AddressNetwork::Regtest,
    }
}

/// Why a string was rejected as a transparent address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransparentAddressError {
    /// The string is not a decodable Zcash address. Carries the parser's reason.
    #[error("not a decodable Zcash address: {0}")]
    Undecodable(String),
    /// The string decodes as a Zcash address, but not a transparent one
    /// (shielded, unified, Sprout, or TEX).
    #[error("not a transparent address: {0}")]
    NotTransparent(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real vectors. The mainnet pair is from `zcash_address`'s own encoding
    // tests; the testnet pair and the shielded / unified / Sprout rejections
    // are the vectors pinned in `zaino-address`'s classifier tests.
    const MAINNET_P2PKH: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
    const MAINNET_P2SH: &str = "t3JZcvsuaXE6ygokL4XUiZSTrQBUoPYFnXJ";
    const TESTNET_P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const TESTNET_P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";
    const SAPLING: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
    const UNIFIED: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    const SPROUT: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";
    const TEX: &str = "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy";

    #[test]
    fn mainnet_p2pkh_accepted() {
        let a = TransparentAddress::try_new(MAINNET_P2PKH).expect("valid mainnet t1");
        assert_eq!(a.network(), AddressNetwork::Mainnet);
        assert_eq!(a.script_type(), ScriptType::P2PKH);
        assert_eq!(a.as_str(), MAINNET_P2PKH);
    }

    #[test]
    fn mainnet_p2sh_accepted() {
        let a = TransparentAddress::try_new(MAINNET_P2SH).expect("valid mainnet t3");
        assert_eq!(a.network(), AddressNetwork::Mainnet);
        assert_eq!(a.script_type(), ScriptType::P2SH);
    }

    #[test]
    fn testnet_p2pkh_accepted() {
        let a = TransparentAddress::try_new(TESTNET_P2PKH).expect("valid testnet tm");
        assert_eq!(a.network(), AddressNetwork::Testnet);
        assert_eq!(a.script_type(), ScriptType::P2PKH);
    }

    #[test]
    fn testnet_p2sh_accepted() {
        let a = TransparentAddress::try_new(TESTNET_P2SH).expect("valid testnet t2");
        assert_eq!(a.network(), AddressNetwork::Testnet);
        assert_eq!(a.script_type(), ScriptType::P2SH);
    }

    #[test]
    fn garbage_is_undecodable() {
        assert!(matches!(
            TransparentAddress::try_new("not an address"),
            Err(TransparentAddressError::Undecodable(_))
        ));
    }

    /// A valid t1 address with its final character mutated: still base58, but
    /// the checksum no longer holds, so it does not decode.
    #[test]
    fn bad_checksum_is_undecodable() {
        let mutated = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbt";
        assert_ne!(mutated, MAINNET_P2PKH);
        assert!(matches!(
            TransparentAddress::try_new(mutated),
            Err(TransparentAddressError::Undecodable(_))
        ));
    }

    #[test]
    fn shielded_is_not_transparent() {
        assert!(matches!(
            TransparentAddress::try_new(SAPLING),
            Err(TransparentAddressError::NotTransparent(_))
        ));
    }

    #[test]
    fn unified_is_not_transparent() {
        assert!(matches!(
            TransparentAddress::try_new(UNIFIED),
            Err(TransparentAddressError::NotTransparent(_))
        ));
    }

    #[test]
    fn sprout_is_not_transparent() {
        assert!(matches!(
            TransparentAddress::try_new(SPROUT),
            Err(TransparentAddressError::NotTransparent(_))
        ));
    }

    #[test]
    fn tex_is_not_transparent() {
        assert!(matches!(
            TransparentAddress::try_new(TEX),
            Err(TransparentAddressError::NotTransparent(_))
        ));
    }

    #[test]
    fn round_trips_through_string_and_display() {
        let a = TransparentAddress::try_new(MAINNET_P2PKH).expect("valid mainnet t1");
        assert_eq!(a.to_string(), MAINNET_P2PKH);
        assert_eq!(String::from(a), MAINNET_P2PKH);
    }
}
