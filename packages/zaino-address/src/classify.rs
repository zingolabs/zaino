//! The two classification entry points.
//!
//! Both parse the supplied string as a [`zcash_address::ZcashAddress`] and then
//! convert it for the queried network, so an address that is well-formed for a
//! *different* network is reported invalid rather than accepted.

use zcash_keys::{address::Address, encoding::AddressCodec as _};
use zcash_protocol::consensus::Parameters;
use zcash_transparent::address::TransparentAddress;

use crate::{
    sapling::sapling_key_bytes,
    validated::{ValidatedAddress, ZValidatedAddress, DEPRECATION_NOTICE},
};

/// Parses `raw_address` for `params`' network, returning `None` if it does not
/// parse or belongs to another network.
///
/// Shared by both entry points so the two RPCs cannot disagree about which
/// addresses exist.
fn parse_for_network<P: Parameters>(raw_address: &str, params: &P) -> Option<Address> {
    let parsed = raw_address.parse::<zcash_address::ZcashAddress>().ok()?;

    match parsed.convert_if_network::<Address>(params.network_type()) {
        Ok(address) => Some(address),
        Err(err) => {
            tracing::debug!(?err, "conversion error");
            None
        }
    }
}

/// Classifies an address for the `validateaddress` RPC.
///
/// Pure address parsing over `params`; no chain data required.
pub fn validate_address<P: Parameters>(raw_address: String, params: &P) -> ValidatedAddress {
    match parse_for_network(&raw_address, params) {
        Some(Address::Transparent(taddr)) => ValidatedAddress::Transparent {
            is_script: matches!(taddr, TransparentAddress::ScriptHash(_)),
            address: raw_address,
        },
        _ => ValidatedAddress::Invalid,
    }
}

/// Classifies an address for the deprecated `z_validateaddress` RPC.
///
/// Pure address parsing over `params`; no chain data required.
///
/// # Deprecation
///
/// Emits [`DEPRECATION_NOTICE`] on every call.
pub fn z_validate_address<P: Parameters>(raw_address: String, params: &P) -> ZValidatedAddress {
    tracing::warn!("{}", DEPRECATION_NOTICE);

    // The transparent arms echo the caller's string; the shielded arms
    // re-encode, because `convert_if_network` has already proved the address
    // belongs to this network and the canonical encoding is what zcashd
    // reports.
    match parse_for_network(&raw_address, params) {
        Some(Address::Transparent(TransparentAddress::PublicKeyHash(_))) => {
            ZValidatedAddress::P2pkh {
                address: raw_address,
            }
        }
        Some(Address::Transparent(TransparentAddress::ScriptHash(_))) => ZValidatedAddress::P2sh {
            address: raw_address,
        },
        Some(Address::Sapling(sapling)) => {
            let (diversifier, diversified_transmission_key) = sapling_key_bytes(&sapling);
            ZValidatedAddress::Sapling {
                address: sapling.encode(params),
                diversifier,
                diversified_transmission_key,
            }
        }
        Some(Address::Unified(unified)) => ZValidatedAddress::Unified {
            address: unified.encode(params),
        },
        // Sprout, and any address kind a future `Address` variant introduces.
        // Reporting "invalid" rather than guessing preserves the previous
        // behaviour and keeps Zaino from claiming to classify what it cannot.
        _ => ZValidatedAddress::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::{NetworkType, Parameters};

    /// A minimal [`Parameters`] carrying only a network type — the classifier
    /// reads nothing else, so the tests need nothing else.
    #[derive(Clone)]
    struct Net(NetworkType);

    impl Parameters for Net {
        fn network_type(&self) -> NetworkType {
            self.0
        }

        fn activation_height(
            &self,
            _nu: zcash_protocol::consensus::NetworkUpgrade,
        ) -> Option<zcash_protocol::consensus::BlockHeight> {
            None
        }
    }

    const TEST: Net = Net(NetworkType::Test);
    const REGTEST: Net = Net(NetworkType::Regtest);
    const MAIN: Net = Net(NetworkType::Main);

    // Canonical source: live-tests/clientless/src/lib.rs::rpc::json_rpc
    // Tracked for DRY consolidation: https://github.com/zingolabs/zaino/issues/988
    const TESTNET_P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const TESTNET_P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";
    const REGTEST_SAPLING: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
    const REGTEST_UNIFIED: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    /// A Sprout address. Zaino does not classify these — see
    /// [`ZValidatedAddress`]'s Sprout note.
    const SPROUT: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";

    #[test]
    fn unparseable_is_invalid() {
        assert_eq!(
            validate_address("not an address".into(), &TEST),
            ValidatedAddress::Invalid
        );
        assert_eq!(
            z_validate_address("not an address".into(), &TEST),
            ZValidatedAddress::Invalid
        );
    }

    /// A well-formed address for the wrong network must not validate. This is
    /// the whole reason classification takes a network rather than parsing in
    /// isolation.
    #[test]
    fn wrong_network_is_invalid() {
        assert_eq!(
            validate_address(TESTNET_P2PKH.into(), &MAIN),
            ValidatedAddress::Invalid
        );
        assert_eq!(
            z_validate_address(TESTNET_P2PKH.into(), &MAIN),
            ZValidatedAddress::Invalid
        );
    }

    #[test]
    fn p2pkh_and_p2sh_are_distinguished() {
        assert_eq!(
            validate_address(TESTNET_P2PKH.into(), &TEST),
            ValidatedAddress::Transparent {
                address: TESTNET_P2PKH.into(),
                is_script: false,
            }
        );
        assert_eq!(
            validate_address(TESTNET_P2SH.into(), &TEST),
            ValidatedAddress::Transparent {
                address: TESTNET_P2SH.into(),
                is_script: true,
            }
        );

        assert_eq!(
            z_validate_address(TESTNET_P2PKH.into(), &TEST),
            ZValidatedAddress::P2pkh {
                address: TESTNET_P2PKH.into()
            }
        );
        assert_eq!(
            z_validate_address(TESTNET_P2SH.into(), &TEST),
            ZValidatedAddress::P2sh {
                address: TESTNET_P2SH.into()
            }
        );
    }

    /// `validateaddress` describes transparent addresses only. A well-formed
    /// shielded address is reported invalid, matching zcashd.
    #[test]
    fn validate_address_rejects_shielded() {
        assert_eq!(
            validate_address(REGTEST_SAPLING.into(), &REGTEST),
            ValidatedAddress::Invalid
        );
        assert_eq!(
            validate_address(REGTEST_UNIFIED.into(), &REGTEST),
            ValidatedAddress::Invalid
        );
    }

    /// The Sapling arm carries key material; the byte-level vector for it is
    /// pinned in [`crate::sapling`]'s tests.
    #[test]
    fn z_validate_address_classifies_shielded() {
        assert!(matches!(
            z_validate_address(REGTEST_SAPLING.into(), &REGTEST),
            ZValidatedAddress::Sapling { .. }
        ));
        assert!(matches!(
            z_validate_address(REGTEST_UNIFIED.into(), &REGTEST),
            ZValidatedAddress::Unified { .. }
        ));
    }

    /// Sprout parses as a Zcash address but Zaino does not classify it, so both
    /// RPCs report invalid rather than describing it.
    #[test]
    fn sprout_is_invalid() {
        for network in [&TEST, &REGTEST, &MAIN] {
            assert_eq!(
                validate_address(SPROUT.into(), network),
                ValidatedAddress::Invalid
            );
            assert_eq!(
                z_validate_address(SPROUT.into(), network),
                ZValidatedAddress::Invalid
            );
        }
    }
}
