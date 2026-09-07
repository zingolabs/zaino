//! Transaction output script, and how it is classified.

/// The standard forms a transparent output script can take.
///
/// A classification, not an encoding. An index that keys outputs by address
/// needs to know which of the two standard forms it is looking at, because the
/// 20 bytes mean different things in each — a public-key hash in one, a script
/// hash in the other — and a non-standard script has no such hash at all.
///
/// Deliberately carries no discriminants. The on-disk tag values belong to
/// whichever backend writes them, so that a second backend is free to choose
/// its own without the vocabulary crate having already decided. A backend maps
/// this to and from its own tags at its persistence boundary.
///
/// `NonStandard` is a real answer, not a failure: such outputs exist on chain
/// and an index must decide what to do with them. What it decides is the
/// index's business — Zaino's transparent-address history keys them, while its
/// UTXO-set accumulator excludes them, mirroring zcashd's `IsUnspendable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptType {
    /// Pay-to-public-key-hash — a `t1...` address.
    P2PKH,
    /// Pay-to-script-hash — a `t3...` address.
    P2SH,
    /// Anything else.
    NonStandard,
}

impl ScriptType {
    /// The classification's name, for diagnostics and wire responses.
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptType::P2PKH => "P2PKH",
            ScriptType::P2SH => "P2SH",
            ScriptType::NonStandard => "NonStandard",
        }
    }
}

/// Classifies a locking script into the 20 bytes an index keys it by, and
/// which script form those bytes came from.
///
/// Total: every script gets a key, including ones with no address. An index
/// that refused non-standard outputs would answer "no history" for an address
/// that has some, so the rule always produces something — but for
/// [`ScriptType::NonStandard`] the 20 bytes are an index key and nothing more.
/// They do not round-trip to a script, and two different non-standard scripts
/// can collide.
///
/// # Why this lives here
///
/// Both halves of the chain have to agree on it. The finalised state applies
/// it while indexing; whatever merges finalised and recent answers applies it
/// to recent outputs, because Zaino's UTXO-set commitment is computed over
/// exactly these bytes. Two implementations that drift produce two different
/// commitments for the same chain, which is a silent wrong answer rather than
/// a failure — and until this existed there were two implementations, which
/// had already drifted.
///
/// # The 21-byte case
///
/// A 21-byte script is read as a leading tag byte followed by a 20-byte hash.
/// That is not a Zcash rule; it is Zaino's, inherited from how the finalised
/// state has always written these rows, and it is reproduced here rather than
/// corrected because on-disk data depends on it. It is a hazard worth naming:
/// a script whose first byte happens to be `0x00` or `0x01` is classified as a
/// standard output and keyed under an address nobody controls. Removing the
/// arm changes what is written, so it belongs with a migration rather than
/// here.
pub fn classify_script(script: &[u8]) -> ([u8; 20], ScriptType) {
    // P2PKH: OP_DUP OP_HASH160 <20> ... OP_EQUALVERIFY OP_CHECKSIG
    const P2PKH_PREFIX: &[u8] = &[0x76, 0xa9, 0x14];
    const P2PKH_SUFFIX: &[u8] = &[0x88, 0xac];
    // P2SH: OP_HASH160 <20> ... OP_EQUAL
    const P2SH_PREFIX: &[u8] = &[0xa9, 0x14];
    const P2SH_SUFFIX: &[u8] = &[0x87];

    let mut hash = [0u8; 20];

    if script.len() == 25 && script.starts_with(P2PKH_PREFIX) && script.ends_with(P2PKH_SUFFIX) {
        hash.copy_from_slice(&script[3..23]);
        return (hash, ScriptType::P2PKH);
    }
    if script.len() == 23 && script.starts_with(P2SH_PREFIX) && script.ends_with(P2SH_SUFFIX) {
        hash.copy_from_slice(&script[2..22]);
        return (hash, ScriptType::P2SH);
    }
    if script.len() == 21 {
        hash.copy_from_slice(&script[1..21]);
        let script_type = match script[0] {
            0x00 => ScriptType::P2PKH,
            0x01 => ScriptType::P2SH,
            _ => ScriptType::NonStandard,
        };
        return (hash, script_type);
    }

    let usable = script.len().min(20);
    hash[..usable].copy_from_slice(&script[..usable]);
    (hash, ScriptType::NonStandard)
}

/// A transparent output script (raw bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Script(Vec<u8>);

impl Script {
    /// Wrap raw script bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<Script> for Vec<u8> {
    fn from(s: Script) -> Self {
        s.0
    }
}

impl From<Vec<u8>> for Script {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p2pkh(hash: [u8; 20]) -> Vec<u8> {
        let mut s = vec![0x76, 0xa9, 0x14];
        s.extend_from_slice(&hash);
        s.extend_from_slice(&[0x88, 0xac]);
        s
    }

    fn p2sh(hash: [u8; 20]) -> Vec<u8> {
        let mut s = vec![0xa9, 0x14];
        s.extend_from_slice(&hash);
        s.push(0x87);
        s
    }

    #[test]
    fn standard_scripts_yield_their_hash() {
        assert_eq!(
            classify_script(&p2pkh([7; 20])),
            ([7; 20], ScriptType::P2PKH)
        );
        assert_eq!(classify_script(&p2sh([9; 20])), ([9; 20], ScriptType::P2SH));
    }

    /// A script of the right shape but the wrong length is not standard.
    #[test]
    fn length_is_part_of_the_pattern() {
        let mut too_long = p2pkh([7; 20]);
        too_long.push(0x00);
        assert_eq!(classify_script(&too_long).1, ScriptType::NonStandard);
    }

    /// Non-standard scripts are keyed, not rejected, and a short one is
    /// zero-padded rather than truncating the key.
    #[test]
    fn non_standard_scripts_are_keyed_by_their_leading_bytes() {
        let (hash, script_type) = classify_script(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(script_type, ScriptType::NonStandard);
        assert_eq!(&hash[..4], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(&hash[4..], &[0u8; 16]);
    }

    #[test]
    fn an_empty_script_is_keyed_by_zeroes() {
        assert_eq!(classify_script(&[]), ([0u8; 20], ScriptType::NonStandard));
    }

    /// The 21-byte arm, pinned because it is a hazard rather than a rule.
    ///
    /// A 21-byte script is read as `tag || hash`, so one beginning `0x00` or
    /// `0x01` is classified standard and keyed under an address nobody
    /// controls — and, being standard, is counted into the UTXO set. This is
    /// reproduced from the finalised state because existing databases were
    /// written with it; asserted here so that removing it is a deliberate act
    /// with a migration attached, not an accident.
    #[test]
    fn a_21_byte_script_is_read_as_tag_and_hash() {
        let mut script = vec![0x00];
        script.extend_from_slice(&[0x42; 20]);
        assert_eq!(classify_script(&script), ([0x42; 20], ScriptType::P2PKH));

        let mut script = vec![0x01];
        script.extend_from_slice(&[0x43; 20]);
        assert_eq!(classify_script(&script), ([0x43; 20], ScriptType::P2SH));

        // Any other leading byte is not a known form, but the hash still comes
        // from after the tag — not from the front of the script.
        let mut script = vec![0x14];
        script.extend_from_slice(&[0x44; 20]);
        assert_eq!(
            classify_script(&script),
            ([0x44; 20], ScriptType::NonStandard)
        );
    }
}
