//! Network type for Zaino configuration.

use std::fmt;

use serde::{Deserialize, Serialize};
use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

/// The network *kind* zaino is configured for. Deliberately payload-free:
/// activation heights are chain facts the validator owns, so a config value
/// cannot carry them — the backends adopt the runtime schedule from the
/// validator's `getblockchaininfo.upgrades` at spawn and hold it as a
/// `zebra_chain::parameters::Network`
/// (<https://github.com/zingolabs/zaino/issues/1076>). A pre-adoption
/// height read is unrepresentable: this type has no heights to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// Testnet network
    Testnet,
    /// Regtest network (for local testing)
    Regtest,
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Network::Mainnet => write!(f, "Mainnet"),
            Network::Testnet => write!(f, "Testnet"),
            Network::Regtest => write!(f, "Regtest"),
        }
    }
}

/// Configurable activation heights for Regtest and configured Testnets.
///
/// We use our own type instead of the zebra type
/// as the zebra type is missing a number of useful
/// traits, notably Debug, PartialEq, and Eq
///
/// This also allows us to define our own set
/// of defaults
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Copy)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct ActivationHeights {
    /// Activation height for `BeforeOverwinter` network upgrade.
    pub before_overwinter: Option<u32>,
    /// Activation height for `Overwinter` network upgrade.
    pub overwinter: Option<u32>,
    /// Activation height for `Sapling` network upgrade.
    pub sapling: Option<u32>,
    /// Activation height for `Blossom` network upgrade.
    pub blossom: Option<u32>,
    /// Activation height for `Heartwood` network upgrade.
    pub heartwood: Option<u32>,
    /// Activation height for `Canopy` network upgrade.
    pub canopy: Option<u32>,
    /// Activation height for `NU5` network upgrade.
    #[serde(rename = "NU5")]
    pub nu5: Option<u32>,
    /// Activation height for `NU6` network upgrade.
    #[serde(rename = "NU6")]
    pub nu6: Option<u32>,
    /// Activation height for `NU6.1` network upgrade.
    /// see <https://zips.z.cash/#nu6-1-candidate-zips> for info on NU6.1
    #[serde(rename = "NU6.1")]
    pub nu6_1: Option<u32>,
    /// Activation height for `NU6.2` network upgrade.
    #[serde(rename = "NU6.2")]
    pub nu6_2: Option<u32>,
    /// Activation height for `NU6.3` network upgrade.
    #[serde(rename = "NU6.3")]
    pub nu6_3: Option<u32>,
    /// Activation height for `NU7` network upgrade.
    #[serde(rename = "NU7")]
    pub nu7: Option<u32>,
}

impl Default for ActivationHeights {
    fn default() -> Self {
        ActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(2),
            nu6: Some(2),
            nu6_1: Some(2),
            nu6_2: Some(2),
            nu6_3: None,
            nu7: None,
        }
    }
}

/// Records the `NetworkUpgrade`-variant ↔ `ActivationHeights`-field correspondence
/// exactly once, generating everything derived from it: the two field-by-field
/// `From` conversions between [`ActivationHeights`] and zebra's
/// [`ConfiguredActivationHeights`] (the structs share field names).
///
/// A declarative macro rather than functions because plain `fn`s cannot abstract
/// over struct fields, and the variant/field spellings (`Nu5`/`nu5`) differ only
/// by casing, which `macro_rules!` cannot derive — hence explicit pairs.
///
/// Zebra's side of these conversions is structurally stable; the recurring edit
/// here is a new network upgrade, which lands as a single `(Variant, field)`
/// entry in the invocation below (after adding the struct field). The exhaustive
/// destructures and match keep full compile-time drift detection: a new zebra
/// field or variant fails the build until its pair is added.
macro_rules! activation_heights_mirror {
    ($(($variant:ident, $field:ident)),* $(,)?) => {
        impl From<ConfiguredActivationHeights> for ActivationHeights {
            fn from(
                ConfiguredActivationHeights { $($field),* }: ConfiguredActivationHeights,
            ) -> Self {
                Self { $($field),* }
            }
        }

        impl From<ActivationHeights> for ConfiguredActivationHeights {
            fn from(ActivationHeights { $($field),* }: ActivationHeights) -> Self {
                Self { $($field),* }
            }
        }
    };
}

activation_heights_mirror!(
    (BeforeOverwinter, before_overwinter),
    (Overwinter, overwinter),
    (Sapling, sapling),
    (Blossom, blossom),
    (Heartwood, heartwood),
    (Canopy, canopy),
    (Nu5, nu5),
    (Nu6, nu6),
    (Nu6_1, nu6_1),
    (Nu6_2, nu6_2),
    (Nu6_3, nu6_3),
    (Nu7, nu7),
);

impl Network {
    /// Determines if we should wait for the server to fully sync. Used for testing
    ///
    /// - Mainnet/Testnet: Skip sync (false) because we don't want to sync real chains in tests
    /// - Regtest: Enable sync (true) because regtest is local and fast to sync
    pub fn wait_on_server_sync(&self) -> bool {
        match self {
            Network::Mainnet | Network::Testnet => false, // Real networks - don't try to sync the whole chain
            Network::Regtest => true,                     // Local network - safe and fast to sync
        }
    }
}

impl From<zebra_chain::parameters::Network> for Network {
    fn from(value: zebra_chain::parameters::Network) -> Self {
        match value {
            zebra_chain::parameters::Network::Mainnet => Network::Mainnet,
            zebra_chain::parameters::Network::Testnet(parameters) => {
                if parameters.is_regtest() {
                    Network::Regtest
                } else {
                    Network::Testnet
                }
            }
        }
    }
}

impl ActivationHeights {
    /// Builds the runtime regtest network for a chain whose schedule is
    /// known first-hand: a validator being *launched* with these heights, or
    /// a test fixture that is its own chain (mockchain sources, proptest
    /// block generators). Production indexer code never calls this with
    /// configured values — it adopts the runtime network from the
    /// validator's reported schedule instead (zaino#1076).
    pub fn to_regtest_network(&self) -> zebra_chain::parameters::Network {
        zebra_chain::parameters::Network::new_regtest(
            Into::<ConfiguredActivationHeights>::into(*self).into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ActivationHeights;

    #[test]
    fn activation_heights_round_trip_nu6_2() {
        let heights = ActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(1),
            nu6: Some(1),
            nu6_1: Some(1),
            nu6_2: Some(2),
            nu6_3: Some(500),
            nu7: Some(1000),
        };

        let zebra_heights: zebra_chain::parameters::testnet::ConfiguredActivationHeights =
            heights.into();
        assert_eq!(zebra_heights.nu6_2, Some(2));
    }
}
