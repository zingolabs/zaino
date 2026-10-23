//! [`AddressKey`]: the identity rows are keyed by, from an address or a locking script

use zcash_transparent::address::TransparentAddress;

pub(super) const HASH160: usize = 20;

/// Standard form an output's 20 bytes came from
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub(super) enum AddressKind {
    P2pkh = 0x00,
    P2sh = 0x01,
    Opaque = 0x02,
}

impl AddressKind {
    pub(super) fn decode(tag: u8) -> Option<Self> {
        match tag {
            0x00 => Some(Self::P2pkh),
            0x01 => Some(Self::P2sh),
            0x02 => Some(Self::Opaque),
            _ => None,
        }
    }
}

/// Fixed-width address tag `[hash160][kind]` (the whole identity a row is keyed by)
///
/// - 21 B = the addr id, never interned (TEX / ephemeral receivers = single-use by construction)
/// - hash first: `receives` scope filter shards on the key's first 8 bytes (must be uniform)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AddressKey {
    pub(super) hash: [u8; HASH160],
    pub(super) kind: AddressKind,
}

impl AddressKey {
    pub(crate) const LEN: usize = 1 + HASH160;

    pub(crate) fn p2pkh(hash: [u8; HASH160]) -> Self {
        Self { kind: AddressKind::P2pkh, hash }
    }

    pub(crate) fn p2sh(hash: [u8; HASH160]) -> Self {
        Self { kind: AddressKind::P2sh, hash }
    }

    /// Every non-standard output, under one key (no address string parses to it)
    pub(crate) fn opaque() -> Self {
        Self { kind: AddressKind::Opaque, hash: [0u8; HASH160] }
    }

    /// - P2PKH `76 a9 14 ‖ h ‖ 88 ac`, P2SH `a9 14 ‖ h ‖ 87`, matched on exact length *and* exact
    ///   prefix/suffix (what makes the rebuilt `TransparentAddress::script()` byte-exact)
    /// - everything else → [`AddressKey::opaque`], still written to `receives` so nothing is lost
    pub(crate) fn of_script(script: &[u8]) -> Self {
        if script.len() == 25
            && script.starts_with(&[0x76, 0xa9, 0x14])
            && script.ends_with(&[0x88, 0xac])
        {
            if let Ok(hash) = <[u8; 20]>::try_from(&script[3..23]) {
                return Self::p2pkh(hash);
            }
        }

        if script.len() == 23 && script.starts_with(&[0xa9, 0x14]) && script.ends_with(&[0x87]) {
            if let Ok(hash) = <[u8; 20]>::try_from(&script[2..22]) {
                return Self::p2sh(hash);
            }
        }

        Self::opaque()
    }
}

impl From<&TransparentAddress> for AddressKey {
    fn from(address: &TransparentAddress) -> Self {
        match *address {
            TransparentAddress::PublicKeyHash(hash) => Self::p2pkh(hash),
            TransparentAddress::ScriptHash(hash) => Self::p2sh(hash),
        }
    }
}

#[cfg(test)]
mod tests {
    use zcash_script::script::Evaluable;

    use super::*;

    /// Queried address keys onto exactly the rows its librustzcash script was stored under (lets
    /// `GetAddressUtxos` return a script it never stored)
    #[test]
    fn a_standard_address_keys_onto_the_script_librustzcash_writes_for_it() {
        let p2pkh = [&[0x76, 0xa9, 0x14][..], &[0x41; 20], &[0x88, 0xac]].concat();
        let p2sh = [&[0xa9, 0x14][..], &[0x42; 20], &[0x87]].concat();

        for (address, script) in [
            (TransparentAddress::PublicKeyHash([0x41; 20]), &p2pkh),
            (TransparentAddress::ScriptHash([0x42; 20]), &p2sh),
        ] {
            assert_eq!(&address.script().to_bytes(), script, "{address:?}: librustzcash script");
            let keyed = AddressKey::of_script(script);
            assert_eq!(keyed, AddressKey::from(&address), "{address:?}: script keys as address");
        }
    }

    /// Classification total: anything not exactly a standard script = opaque
    #[test]
    fn every_non_standard_script_is_opaque() {
        let p2pkh = |hash: [u8; 20]| [&[0x76, 0xa9, 0x14][..], &hash, &[0x88, 0xac]].concat();
        let too_long = [p2pkh([0x41; 20]), vec![0x00]].concat();
        let wrong_suffix = [&p2pkh([0x41; 20])[..24], &[0xad]].concat();
        #[rustfmt::skip]
        let cases = [
            ("p2pkh shape, one byte too long", too_long),
            ("p2pkh ending OP_CHECKSIGVERIFY", wrong_suffix),
            // keying it as standard = an address nobody controls
            ("[tag][hash20]", [&[0x00u8][..], &[0x43; 20]].concat()),
            ("empty", vec![]),
            ("4 arbitrary bytes", vec![0xde, 0xad, 0xbe, 0xef]),
            ("80 × OP_RETURN", vec![0x6a; 80]),
        ];

        for (case, script) in cases {
            assert_eq!(
                AddressKey::of_script(&script),
                AddressKey::opaque(),
                "{case}: {script:02x?}"
            );
        }
    }
}
