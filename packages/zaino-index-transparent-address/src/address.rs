//! Locking script → [`AddressKey`]

use crate::key::AddressKey;

/// - P2PKH `76 a9 14 ‖ h ‖ 88 ac`, P2SH `a9 14 ‖ h ‖ 87`, matched on exact length *and* exact
///   prefix/suffix (what makes `AddressKey::script()` byte-exact)
/// - everything else → [`AddressKey::opaque`], still written to `receives` so nothing is lost
pub(crate) fn address_key(script: &[u8]) -> AddressKey {
    if script.len() == 25
        && script.starts_with(&[0x76, 0xa9, 0x14])
        && script.ends_with(&[0x88, 0xac])
    {
        if let Ok(hash) = <[u8; 20]>::try_from(&script[3..23]) {
            return AddressKey::p2pkh(hash);
        }
    }

    if script.len() == 23 && script.starts_with(&[0xa9, 0x14]) && script.ends_with(&[0x87]) {
        if let Ok(hash) = <[u8; 20]>::try_from(&script[2..22]) {
            return AddressKey::p2sh(hash);
        }
    }

    AddressKey::opaque()
}

#[cfg(test)]
mod tests {
    use zcash_script::script::Evaluable;
    use zcash_transparent::address::TransparentAddress;

    use super::*;

    /// Classification total; a queried address keys onto exactly the rows its librustzcash script
    /// was stored under (lets `GetAddressUtxos` return a script it never stored)
    #[test]
    fn standard_scripts_round_trip_and_everything_else_is_opaque() {
        let p2pkh = [&[0x76, 0xa9, 0x14][..], &[0x41; 20], &[0x88, 0xac]].concat();
        let p2sh = [&[0xa9, 0x14][..], &[0x42; 20], &[0x87]].concat();

        for (address, script) in [
            (TransparentAddress::PublicKeyHash([0x41; 20]), &p2pkh),
            (TransparentAddress::ScriptHash([0x42; 20]), &p2sh),
        ] {
            assert_eq!(&address.script().to_bytes(), script, "{address:?}");
            assert_eq!(address_key(script), AddressKey::from(&address), "{address:?}");
        }

        // right shape, wrong length: not standard, whatever the prefix
        let mut too_long = p2pkh.clone();
        too_long.push(0x00);
        assert_eq!(address_key(&too_long), AddressKey::opaque());

        let mut wrong_suffix = p2pkh.clone();
        wrong_suffix[24] = 0xad;
        assert_eq!(address_key(&wrong_suffix), AddressKey::opaque());

        // `[tag][hash20]`: opaque (keying it as standard = an address nobody controls)
        let tagged = [&[0x00u8][..], &[0x43; 20]].concat();
        assert_eq!(address_key(&tagged), AddressKey::opaque());

        for script in [&[][..], &[0xde, 0xad, 0xbe, 0xef][..], &[0x6a; 80][..]] {
            assert_eq!(address_key(script), AddressKey::opaque());
        }
    }
}
