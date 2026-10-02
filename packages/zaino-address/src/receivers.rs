//! Unified-address receiver decomposition.
//!
//! `z_listunifiedreceivers` takes a unified address and reports each receiver it
//! bundles, re-encoded as a standalone address. Like the two validation
//! entry points, it reads no chain state: it is a pure function of the address
//! string and the network.
//!
//! Each receiver is re-encoded in the form a caller can actually pay to: the
//! transparent and Sapling receivers have standalone encodings of their own,
//! while an Orchard receiver does not — the only way to address it is a unified
//! address carrying just that receiver, which is what this reports.

use zcash_keys::address::{Address, UnifiedAddress};
use zcash_keys::encoding::AddressCodec as _;
use zcash_protocol::consensus::Parameters;
use zcash_transparent::address::TransparentAddress;

/// The receivers a unified address bundles, each re-encoded standalone.
///
/// A field is `None` when the unified address carries no receiver of that kind.
/// Only the kinds Zaino re-encodes are modelled; an unknown or unsupported
/// receiver type is omitted rather than reported as an opaque blob.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnifiedReceivers {
    /// Orchard receiver, as a unified address containing only it.
    pub orchard: Option<String>,
    /// Sapling receiver, as a Sapling payment address.
    pub sapling: Option<String>,
    /// Transparent pay-to-public-key-hash receiver.
    pub p2pkh: Option<String>,
    /// Transparent pay-to-script-hash receiver.
    pub p2sh: Option<String>,
}

/// Decompose a unified address into its receivers.
///
/// `None` when `raw_address` is not a unified address for `params`' network —
/// a definitive answer, since the caller asked about a specific string.
pub fn list_unified_receivers<P: Parameters>(
    raw_address: String,
    params: &P,
) -> Option<UnifiedReceivers> {
    let parsed = raw_address.parse::<zcash_address::ZcashAddress>().ok()?;
    let unified = match parsed.convert_if_network::<Address>(params.network_type()) {
        Ok(Address::Unified(unified)) => unified,
        Ok(_) => return None,
        Err(err) => {
            tracing::debug!(?err, "conversion error");
            return None;
        }
    };

    let orchard = unified.orchard().copied().and_then(|receiver| {
        // An Orchard receiver has no standalone encoding, so it is reported as a
        // unified address containing only itself. `from_receivers` returns
        // `None` only without a shielded receiver, which cannot happen here.
        UnifiedAddress::from_receivers(Some(receiver), None, None)
            .map(|only_orchard| only_orchard.encode(params))
    });
    let sapling = unified.sapling().map(|receiver| receiver.encode(params));
    let (p2pkh, p2sh) = match unified.transparent() {
        Some(TransparentAddress::PublicKeyHash(_)) => {
            (unified.transparent().map(|t| t.encode(params)), None)
        }
        Some(TransparentAddress::ScriptHash(_)) => {
            (None, unified.transparent().map(|t| t.encode(params)))
        }
        None => (None, None),
    };

    Some(UnifiedReceivers {
        orchard,
        sapling,
        p2pkh,
        p2sh,
    })
}

#[cfg(test)]
mod tests {
    use super::list_unified_receivers;
    use zcash_protocol::consensus::Network;

    /// A string that is not a unified address has no receivers to list. This is
    /// a definitive answer, not a failure.
    #[test]
    fn a_non_unified_address_has_no_receivers() {
        let got = list_unified_receivers("t1notunified".to_string(), &Network::MainNetwork);
        assert!(got.is_none());
    }

    /// A unified address reports each receiver it bundles.
    ///
    /// The vector is the mainnet unified address this repo already exercises in
    /// `packages/zaino-state/src/config.rs`, so the test depends on no fixture
    /// this crate has to maintain.
    #[test]
    fn a_unified_address_lists_its_receivers() {
        let ua = "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6\
                  drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666k\
                  gwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw\
                  6uh7hyvzjse8ksx"
            .to_string();
        let got = list_unified_receivers(ua, &Network::MainNetwork)
            .expect("a unified address decomposes");
        assert!(
            got.orchard.is_some() || got.sapling.is_some(),
            "a unified address bundles at least one shielded receiver"
        );
    }

    /// The Orchard receiver must actually be reported. Without the `orchard`
    /// feature on `zcash_keys`, `has_orchard()` returns `false` rather than
    /// failing to compile, so a misconfigured build answers "no Orchard
    /// receiver" for an address that has one. This test is what catches that.
    #[test]
    fn an_orchard_receiver_is_reported_not_silently_dropped() {
        let ua = "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6\
                  drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666k\
                  gwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw\
                  6uh7hyvzjse8ksx"
            .to_string();
        let got = list_unified_receivers(ua, &Network::MainNetwork)
            .expect("a unified address decomposes");
        assert!(
            got.orchard.is_some(),
            "this vector carries an Orchard receiver; a `false` here means the \
             zcash_keys `orchard` feature is not enabled"
        );
    }
}
