//! Decode a transparent output script to the address it pays.
//!
//! The explorer's `scriptPubKey.addresses` is the address a transparent output
//! locks funds to, and `scriptPubKey.type` is which standard template it is. Only
//! the two standard templates carry an address: pay-to-public-key-hash and
//! pay-to-script-hash. Every other script — a bare multisig, an `OP_RETURN`, a
//! malformed standard template — is not an address, and the caller renders no
//! `addresses`/`type` for it.
//!
//! The address and its kind are decided here together, in one pass, so a caller
//! never re-inspects the script bytes to label what this already classified.

use zcash_keys::encoding::AddressCodec as _;
use zcash_protocol::consensus::Parameters;
use zcash_transparent::address::TransparentAddress;

/// P2PKH template: `OP_DUP OP_HASH160 <20-byte push> OP_EQUALVERIFY OP_CHECKSIG`.
const P2PKH_PREFIX: [u8; 3] = [0x76, 0xa9, 0x14];
const P2PKH_SUFFIX: [u8; 2] = [0x88, 0xac];
/// P2SH template: `OP_HASH160 <20-byte push> OP_EQUAL`.
const P2SH_PREFIX: [u8; 2] = [0xa9, 0x14];
const P2SH_SUFFIX: [u8; 1] = [0x87];

/// Which standard transparent template a script is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransparentScriptKind {
    /// Pay-to-public-key-hash — a `t1…` address.
    PubKeyHash,
    /// Pay-to-script-hash — a `t3…` address.
    ScriptHash,
}

/// A decoded transparent output script: the address it pays and which standard
/// template it is. The two travel together so a caller labels the address kind
/// without re-reading the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptAddress {
    /// Which standard template the script is.
    pub kind: TransparentScriptKind,
    /// The address, encoded for the queried network.
    pub address: String,
}

/// Decodes a standard pay-to-public-key-hash or pay-to-script-hash locking
/// script to the address it pays on `params`' network and its kind, or `None`
/// for any other script.
///
/// Strict: only the exact 25-byte P2PKH and 23-byte P2SH templates decode. This
/// is deliberately narrower than [`zaino_primitives`'s `classify_script`], which
/// keys *every* script (including non-standard ones) for indexing and reads a
/// 21-byte script as a bare `tag || hash`. An address is a user-facing claim
/// about who controls an output, so the two lenient cases that index fine are
/// `None` here rather than an address nobody can verify.
pub fn transparent_address_from_script<P: Parameters>(
    script: &[u8],
    params: &P,
) -> Option<ScriptAddress> {
    let (kind, address) = transparent_address(script)?;
    Some(ScriptAddress {
        kind,
        address: address.encode(params),
    })
}

/// The standard transparent address a script locks to and its kind, independent
/// of network.
fn transparent_address(script: &[u8]) -> Option<(TransparentScriptKind, TransparentAddress)> {
    if let Some(hash) = template_hash(script, &P2PKH_PREFIX, &P2PKH_SUFFIX) {
        return Some((
            TransparentScriptKind::PubKeyHash,
            TransparentAddress::PublicKeyHash(hash),
        ));
    }
    if let Some(hash) = template_hash(script, &P2SH_PREFIX, &P2SH_SUFFIX) {
        return Some((
            TransparentScriptKind::ScriptHash,
            TransparentAddress::ScriptHash(hash),
        ));
    }
    None
}

/// The 20-byte hash a script carries when it matches `prefix <20 bytes> suffix`
/// exactly, or `None` when it does not fit that template.
fn template_hash(script: &[u8], prefix: &[u8], suffix: &[u8]) -> Option<[u8; 20]> {
    if script.len() != prefix.len() + 20 + suffix.len()
        || !script.starts_with(prefix)
        || !script.ends_with(suffix)
    {
        return None;
    }
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&script[prefix.len()..prefix.len() + 20]);
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::Network;

    /// Build the canonical P2PKH locking script for a 20-byte hash.
    fn p2pkh_script(hash: [u8; 20]) -> Vec<u8> {
        let mut script = P2PKH_PREFIX.to_vec();
        script.extend_from_slice(&hash);
        script.extend_from_slice(&P2PKH_SUFFIX);
        script
    }

    /// Build the canonical P2SH locking script for a 20-byte hash.
    fn p2sh_script(hash: [u8; 20]) -> Vec<u8> {
        let mut script = P2SH_PREFIX.to_vec();
        script.extend_from_slice(&hash);
        script.extend_from_slice(&P2SH_SUFFIX);
        script
    }

    /// The 20-byte hash inside a real address, extracted with the library
    /// decoder so the vectors below are genuine on-chain encodings rather than
    /// numbers this test invented.
    fn hash_of(address: &str, params: &Network) -> [u8; 20] {
        match TransparentAddress::decode(params, address).expect("a valid address") {
            TransparentAddress::PublicKeyHash(hash) | TransparentAddress::ScriptHash(hash) => hash,
        }
    }

    // Real testnet vectors, captured from zcashd's `z_validateaddress`
    // (shared with `zaino-serve` wire::address::tests). The mainnet vectors are
    // the same two hashes re-encoded for mainnet.
    const TESTNET_P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const TESTNET_P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";

    const TESTNET: Network = Network::TestNetwork;
    const MAINNET: Network = Network::MainNetwork;

    /// A P2PKH script decodes to the pay-to-public-key-hash address on both
    /// networks, and the same script decodes to different strings per network —
    /// which is why the decoder takes `params` rather than classifying in
    /// isolation.
    #[test]
    fn p2pkh_script_decodes_to_the_network_address() {
        let hash = hash_of(TESTNET_P2PKH, &TESTNET);
        let script = p2pkh_script(hash);

        let testnet =
            transparent_address_from_script(&script, &TESTNET).expect("a valid testnet address");
        assert_eq!(testnet.kind, TransparentScriptKind::PubKeyHash);
        assert_eq!(testnet.address, TESTNET_P2PKH);

        // Mainnet re-encodes the identical hash under the mainnet P2PKH prefix.
        let mainnet = transparent_address_from_script(&script, &MAINNET)
            .expect("the same hash is a valid mainnet address");
        assert_eq!(mainnet.kind, TransparentScriptKind::PubKeyHash);
        assert!(
            mainnet.address.starts_with("t1"),
            "mainnet P2PKH is a t1 address: {}",
            mainnet.address
        );
        assert_eq!(hash_of(&mainnet.address, &MAINNET), hash);
        assert_ne!(mainnet.address.as_str(), TESTNET_P2PKH);
    }

    /// A P2SH script decodes to the pay-to-script-hash address on both networks.
    #[test]
    fn p2sh_script_decodes_to_the_network_address() {
        let hash = hash_of(TESTNET_P2SH, &TESTNET);
        let script = p2sh_script(hash);

        let testnet =
            transparent_address_from_script(&script, &TESTNET).expect("a valid testnet address");
        assert_eq!(testnet.kind, TransparentScriptKind::ScriptHash);
        assert_eq!(testnet.address, TESTNET_P2SH);

        let mainnet = transparent_address_from_script(&script, &MAINNET)
            .expect("the same hash is a valid mainnet address");
        assert_eq!(mainnet.kind, TransparentScriptKind::ScriptHash);
        assert!(
            mainnet.address.starts_with("t3"),
            "mainnet P2SH is a t3 address: {}",
            mainnet.address
        );
        assert_eq!(hash_of(&mainnet.address, &MAINNET), hash);
    }

    /// Non-standard scripts are not addresses, so they decode to `None` rather
    /// than to a fabricated address. A P2PKH template of the wrong length, a
    /// template with a corrupted opcode, an empty script, and a bare data push
    /// are all `None`.
    #[test]
    fn non_standard_scripts_decode_to_none() {
        let hash = [0x11u8; 20];

        // Right shape, one byte too long.
        let mut too_long = p2pkh_script(hash);
        too_long.push(0x00);
        assert!(transparent_address_from_script(&too_long, &TESTNET).is_none());

        // Right length, wrong final opcode (OP_CHECKSIG -> OP_CHECKMULTISIG).
        let mut wrong_opcode = p2pkh_script(hash);
        *wrong_opcode.last_mut().expect("non-empty") = 0xae;
        assert!(transparent_address_from_script(&wrong_opcode, &TESTNET).is_none());

        // The 21-byte `tag || hash` form `classify_script` keys for indexing is
        // not a standard template, so it is not an address.
        let mut tagged = vec![0x00u8];
        tagged.extend_from_slice(&hash);
        assert!(transparent_address_from_script(&tagged, &TESTNET).is_none());

        assert!(transparent_address_from_script(&[], &TESTNET).is_none());
        assert!(transparent_address_from_script(&[0xde, 0xad, 0xbe, 0xef], &TESTNET).is_none());
    }
}
