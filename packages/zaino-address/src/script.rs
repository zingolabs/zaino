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

/// Disassemble a transparent script to zcashd's `asm` string (best-effort).
///
/// Renders each opcode in the order the script lists them: a data push as the hex
/// of its bytes, the small-number opcodes (`OP_0`, `OP_1NEGATE`, `OP_1`..`OP_16`)
/// as their decimal value, and every named opcode as `OP_NAME`. This matches
/// zcashd's `ScriptToAsmStr` for the standard templates the explorer renders
/// (P2PKH, P2SH); an opcode outside the table below renders as `OP_UNKNOWN`, and
/// a truncated push renders a trailing `[error]`, both best-effort — a
/// non-standard script's `asm` is informational and not parsed by any consumer.
///
/// `scriptSig` is a signature script, so a data push that parses as a DER
/// signature has its trailing sighash-type byte decoded as a `[ALL]` / `[NONE]`
/// annotation, matching zcashd's `fAttemptSighashDecode`. A `scriptPubKey` (a
/// locking script, never signatures) passes `decode_sighash = false`.
pub fn script_to_asm(script: &[u8], decode_sighash: bool) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < script.len() {
        let opcode = script[i];
        i += 1;
        match opcode {
            // A direct push of `opcode` bytes (OP_PUSHBYTES_1..75).
            0x01..=0x4b => {
                let len = usize::from(opcode);
                push_token(&mut tokens, script, &mut i, len, decode_sighash);
            }
            // OP_PUSHDATA1/2/4: a length prefix of 1, 2 or 4 little-endian bytes.
            0x4c..=0x4e => {
                let width = match opcode {
                    0x4c => 1,
                    0x4d => 2,
                    _ => 4,
                };
                match read_le_len(script, i, width) {
                    Some(len) => {
                        i += width;
                        push_token(&mut tokens, script, &mut i, len, decode_sighash);
                    }
                    None => {
                        tokens.push("[error]".to_string());
                        break;
                    }
                }
            }
            0x00 => tokens.push("0".to_string()),
            0x4f => tokens.push("-1".to_string()),
            // OP_1..OP_16 render as their decimal value.
            0x51..=0x60 => tokens.push((u16::from(opcode) - 0x50).to_string()),
            other => tokens.push(opcode_name(other).to_string()),
        }
    }
    tokens.join(" ")
}

/// Push one data token: the hex of `len` bytes starting at `*i`, advancing `*i`.
/// With `decode_sighash`, a DER signature's trailing sighash byte is split off
/// into a `[ALL]`-style annotation. A push that runs past the script end renders
/// `[error]` best-effort rather than panicking.
fn push_token(
    tokens: &mut Vec<String>,
    script: &[u8],
    i: &mut usize,
    len: usize,
    decode_sighash: bool,
) {
    let Some(data) = script.get(*i..*i + len) else {
        tokens.push("[error]".to_string());
        *i = script.len();
        return;
    };
    *i += len;
    if decode_sighash {
        if let Some((der, annotation)) = split_sighash(data) {
            tokens.push(format!("{}{annotation}", hex_bytes(der)));
            return;
        }
    }
    tokens.push(hex_bytes(data));
}

/// Read a `width`-byte little-endian length at `offset`, or `None` if truncated.
fn read_le_len(script: &[u8], offset: usize, width: usize) -> Option<usize> {
    let bytes = script.get(offset..offset + width)?;
    let mut len = 0usize;
    for (shift, byte) in bytes.iter().enumerate() {
        len |= usize::from(*byte) << (8 * shift);
    }
    Some(len)
}

/// If `data` is a DER-encoded ECDSA signature followed by a known sighash-type
/// byte, return the signature bytes without that byte and its `[..]` annotation.
///
/// The heuristic matches the standard case zcashd decodes: a push beginning with
/// the DER sequence tag `0x30` whose declared length accounts for every byte but
/// the trailing sighash type, and whose last byte is one of the six sighash
/// types. A public-key push (tag `0x02`/`0x03`/`0x04`) never matches, so it
/// renders as plain hex.
fn split_sighash(data: &[u8]) -> Option<(&[u8], &'static str)> {
    let (&tag, rest) = data.split_first()?;
    if tag != 0x30 || rest.is_empty() {
        return None;
    }
    // rest[0] is the DER content length; the signature is tag + len + content,
    // and the push is that plus one trailing sighash-type byte.
    let content_len = usize::from(rest[0]);
    if data.len() != content_len + 3 {
        return None;
    }
    let (sig, hash_type) = data.split_at(data.len() - 1);
    let annotation = match hash_type[0] {
        0x01 => "[ALL]",
        0x02 => "[NONE]",
        0x03 => "[SINGLE]",
        0x81 => "[ALL|ANYONECANPAY]",
        0x82 => "[NONE|ANYONECANPAY]",
        0x83 => "[SINGLE|ANYONECANPAY]",
        _ => return None,
    };
    Some((sig, annotation))
}

/// Lowercase hex of a byte slice.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// zcashd's `GetOpName` for the opcodes that appear in standard scripts, plus the
/// common verification and hashing opcodes. An opcode outside this table is
/// `OP_UNKNOWN` — best-effort, since only informational `asm` reaches here.
fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        0x61 => "OP_NOP",
        0x63 => "OP_IF",
        0x64 => "OP_NOTIF",
        0x67 => "OP_ELSE",
        0x68 => "OP_ENDIF",
        0x69 => "OP_VERIFY",
        0x6a => "OP_RETURN",
        0x6d => "OP_2DROP",
        0x75 => "OP_DROP",
        0x76 => "OP_DUP",
        0x78 => "OP_OVER",
        0x7c => "OP_SWAP",
        0x82 => "OP_SIZE",
        0x87 => "OP_EQUAL",
        0x88 => "OP_EQUALVERIFY",
        0x93 => "OP_ADD",
        0x94 => "OP_SUB",
        0xa6 => "OP_RIPEMD160",
        0xa7 => "OP_SHA1",
        0xa8 => "OP_SHA256",
        0xa9 => "OP_HASH160",
        0xaa => "OP_HASH256",
        0xac => "OP_CHECKSIG",
        0xad => "OP_CHECKSIGVERIFY",
        0xae => "OP_CHECKMULTISIG",
        0xaf => "OP_CHECKMULTISIGVERIFY",
        _ => "OP_UNKNOWN",
    }
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

    /// Decode a lowercase hex string to bytes, for the oracle asm vectors.
    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// A standard P2PKH / P2SH locking script disassembles to zcashd's `asm`
    /// string — the exact strings the zebra oracle emits for block 3,504,000's
    /// coinbase outputs. `scriptPubKey` is never a signature, so sighash decoding
    /// is off.
    #[test]
    fn scriptpubkey_disassembles_to_oracle_asm() {
        let p2pkh = from_hex("76a91425db9091e9786867e536f70089a5102523ab1d6a88ac");
        assert_eq!(
            script_to_asm(&p2pkh, false),
            "OP_DUP OP_HASH160 25db9091e9786867e536f70089a5102523ab1d6a OP_EQUALVERIFY OP_CHECKSIG"
        );
        let p2sh = from_hex("a914c20cd5bdf7964ca61764db66bc2531b1792a084d87");
        assert_eq!(
            script_to_asm(&p2sh, false),
            "OP_HASH160 c20cd5bdf7964ca61764db66bc2531b1792a084d OP_EQUAL"
        );
    }

    /// A P2PKH `scriptSig` disassembles to the oracle's asm: the DER signature
    /// with its trailing sighash byte decoded as `[ALL]`, then the public key as
    /// plain hex. Pins the `fAttemptSighashDecode` path.
    #[test]
    fn scriptsig_decodes_the_sighash_type() {
        let script_sig = from_hex(
            "483045022100f26bfeb03db727cb5705f8a4f293b4c7e4aa06346b9c76fa7f49cc865596490b02207dd3d36721884455b551a2a39c188000f56961811ad2bcd64b8e1830d751d2d6012103c67921dc0d5c0ae1cc2b4d3162fe6b37a856b928ba29a503cdb039e81f1158fb",
        );
        assert_eq!(
            script_to_asm(&script_sig, true),
            "3045022100f26bfeb03db727cb5705f8a4f293b4c7e4aa06346b9c76fa7f49cc865596490b02207dd3d36721884455b551a2a39c188000f56961811ad2bcd64b8e1830d751d2d6[ALL] 03c67921dc0d5c0ae1cc2b4d3162fe6b37a856b928ba29a503cdb039e81f1158fb"
        );
    }

    /// An `OP_RETURN` data carrier disassembles best-effort: the opcode name then
    /// the pushed data as hex. A public-key push is not mistaken for a signature.
    #[test]
    fn op_return_and_pubkey_render_best_effort() {
        let op_return = from_hex("6a04deadbeef");
        assert_eq!(script_to_asm(&op_return, false), "OP_RETURN deadbeef");
        // A bare 33-byte pubkey push, with sighash decoding on, stays plain hex.
        let mut pubkey_push = vec![0x21u8];
        pubkey_push.extend_from_slice(&[0x03; 33]);
        assert_eq!(script_to_asm(&pubkey_push, true), "03".repeat(33));
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
