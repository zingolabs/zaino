//! A transparent address string, as the key the address index is written
//! under.
//!
//! The address-history index keys receives by `(script_type, hash160)`, derived
//! on the write side from the output script by
//! [`classify_script`](zaino_primitives::types::classify_script). A reader
//! holding a queried t-address needs the same pair, which is what
//! [`transparent_address_key`] produces — so write and read agree without the
//! index itself depending on an address parser.
//!
//! # Network
//!
//! The pair is **network-independent**: a t-address encodes its network in the
//! base58 prefix, but the hash it locks to is the same bytes on any network,
//! and that hash is what the index stores. So this accepts an address for any
//! network and reports the key it would be written under. Deciding whether an
//! address belongs to the network being served is the serving adapter's job —
//! it is the layer that knows which network this is; see
//! [`validate_address`](crate::validate_address), which does check.

use zaino_primitives::types::{ScriptType, TransparentAddress};
use zcash_address::{ConversionError, TryFromAddress, ZcashAddress};
use zcash_protocol::consensus::NetworkType;

/// The index key a transparent address resolves to.
///
/// A private carrier for [`TryFromAddress`]: the trait dispatches on the
/// address kind, so each transparent arm records which script shape it came
/// from. Every other arm keeps the trait's default, which reports the kind as
/// unsupported — that is how a shielded or unified address becomes `None`.
struct TransparentKey {
    script_type: ScriptType,
    hash: [u8; 20],
}

impl TryFromAddress for TransparentKey {
    type Error = core::convert::Infallible;

    fn try_from_transparent_p2pkh(
        _net: NetworkType,
        data: [u8; 20],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            script_type: ScriptType::P2PKH,
            hash: data,
        })
    }

    fn try_from_transparent_p2sh(
        _net: NetworkType,
        data: [u8; 20],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Self {
            script_type: ScriptType::P2SH,
            hash: data,
        })
    }
}

/// The `(script_type, hash160)` an address-history read looks `address` up by,
/// or `None` if it is not a transparent address.
///
/// `None` covers every non-transparent kind — shielded, unified, and TEX, which
/// encodes a transparent hash but under a distinct kind whose spend rules this
/// lookup does not model.
pub fn transparent_address_key(address: &TransparentAddress) -> Option<(ScriptType, [u8; 20])> {
    let parsed: ZcashAddress = address.as_str().parse().ok()?;
    let key: TransparentKey = parsed.convert().ok()?;
    Some((key.script_type, key.hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::classify_script;

    // Canonical source: `crate::classify`'s test vectors.
    const TESTNET_P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const TESTNET_P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";
    const REGTEST_SAPLING: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";

    fn key(address: &str) -> Option<(ScriptType, [u8; 20])> {
        transparent_address_key(&TransparentAddress::new(address.to_owned()))
    }

    #[test]
    fn transparent_addresses_resolve_to_their_script_shape() {
        let (kind, hash) = key(TESTNET_P2PKH).expect("a p2pkh address");
        assert_eq!(kind, ScriptType::P2PKH);
        let (kind, script_hash) = key(TESTNET_P2SH).expect("a p2sh address");
        assert_eq!(kind, ScriptType::P2SH);
        assert_ne!(hash, script_hash, "distinct addresses, distinct hashes");
    }

    /// The point of the whole module: the key a *read* derives from the address
    /// string is the key a *write* derived from the output script paying it.
    #[test]
    fn the_read_key_matches_what_classify_script_writes() {
        let (kind, hash) = key(TESTNET_P2PKH).expect("a p2pkh address");
        let mut script = vec![0x76, 0xa9, 0x14];
        script.extend_from_slice(&hash);
        script.extend_from_slice(&[0x88, 0xac]);
        assert_eq!(classify_script(&script), (hash, kind));

        let (kind, hash) = key(TESTNET_P2SH).expect("a p2sh address");
        let mut script = vec![0xa9, 0x14];
        script.extend_from_slice(&hash);
        script.push(0x87);
        assert_eq!(classify_script(&script), (hash, kind));
    }

    #[test]
    fn non_transparent_and_malformed_addresses_have_no_key() {
        assert!(key(REGTEST_SAPLING).is_none(), "shielded");
        assert!(key("not an address").is_none(), "malformed");
        assert!(key("").is_none(), "empty");
    }
}
