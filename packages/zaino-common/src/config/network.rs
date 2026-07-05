//! Network type for Zaino configuration.

use std::fmt;

use serde::{Deserialize, Serialize};
use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

/// Must equal zcash_local_net's `supported_regtest_activation_heights`: the
/// zcash-devtool wallet client hardcodes that canonical set (NU6.3 at 2), and a
/// zebrad configured differently rejects wallet-built transactions with
/// "incorrect consensus branch id". See
/// <https://github.com/zingolabs/zaino/issues/1368>.
pub const ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS: ActivationHeights = ActivationHeights {
    overwinter: Some(1),
    before_overwinter: Some(1),
    sapling: Some(1),
    blossom: Some(1),
    heartwood: Some(1),
    canopy: Some(1),
    nu5: Some(2),
    nu6: Some(2),
    nu6_1: Some(2),
    nu6_2: Some(2),
    nu6_3: Some(2),
    nu7: None,
};

/// Network type for Zaino configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(from = "NetworkSerde", into = "NetworkSerde")]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// Testnet network
    Testnet,
    /// Regtest network (for local testing)
    Regtest(ActivationHeights),
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Network::Mainnet => write!(f, "Mainnet"),
            Network::Testnet => write!(f, "Testnet"),
            Network::Regtest(_) => write!(f, "Regtest"),
        }
    }
}

/// Helper type for Network serialization/deserialization.
///
/// This allows Network to serialize as simple strings ("Mainnet", "Testnet", "Regtest")
/// while the actual Network::Regtest variant carries activation heights internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum NetworkSerde {
    Mainnet,
    Testnet,
    Regtest,
}

impl From<NetworkSerde> for Network {
    fn from(value: NetworkSerde) -> Self {
        match value {
            NetworkSerde::Mainnet => Network::Mainnet,
            NetworkSerde::Testnet => Network::Testnet,
            NetworkSerde::Regtest => Network::Regtest(ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS),
        }
    }
}

impl From<Network> for NetworkSerde {
    fn from(value: Network) -> Self {
        match value {
            Network::Mainnet => NetworkSerde::Mainnet,
            Network::Testnet => NetworkSerde::Testnet,
            Network::Regtest(_) => NetworkSerde::Regtest,
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
/// [`ConfiguredActivationHeights`] (the structs share field names), and
/// `ActivationHeights::from_zebra_pairs`, which walks zebra's
/// `(height, upgrade)` activation list.
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

        impl ActivationHeights {
            /// Builds heights from zebra's `(height, upgrade)` activation list;
            /// upgrades absent from the list stay `None`.
            fn from_zebra_pairs<'a>(
                pairs: impl IntoIterator<
                    Item = (
                        &'a zebra_chain::block::Height,
                        &'a zebra_chain::parameters::NetworkUpgrade,
                    ),
                >,
            ) -> Self {
                let mut heights = Self { $($field: None),* };
                for (height, upgrade) in pairs {
                    match upgrade {
                        zebra_chain::parameters::NetworkUpgrade::Genesis => (),
                        $(
                            zebra_chain::parameters::NetworkUpgrade::$variant => {
                                heights.$field = Some(height.0)
                            }
                        )*
                    }
                }
                heights
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
    /// Convert to Zebra's network type for internal use (alias for to_zebra_default).
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        self.into()
    }

    /// Determines if we should wait for the server to fully sync. Used for testing
    ///
    /// - Mainnet/Testnet: Skip sync (false) because we don't want to sync real chains in tests
    /// - Regtest: Enable sync (true) because regtest is local and fast to sync
    pub fn wait_on_server_sync(&self) -> bool {
        match self {
            Network::Mainnet | Network::Testnet => false, // Real networks - don't try to sync the whole chain
            Network::Regtest(_) => true,                  // Local network - safe and fast to sync
        }
    }

    pub fn from_network_kind_and_activation_heights(
        network: &zebra_chain::parameters::NetworkKind,
        activation_heights: &ActivationHeights,
    ) -> Self {
        match network {
            zebra_chain::parameters::NetworkKind::Mainnet => Network::Mainnet,
            zebra_chain::parameters::NetworkKind::Testnet => Network::Testnet,
            zebra_chain::parameters::NetworkKind::Regtest => Network::Regtest(*activation_heights),
        }
    }
}

impl From<zebra_chain::parameters::Network> for Network {
    fn from(value: zebra_chain::parameters::Network) -> Self {
        match value {
            zebra_chain::parameters::Network::Mainnet => Network::Mainnet,
            zebra_chain::parameters::Network::Testnet(parameters) => {
                if parameters.is_regtest() {
                    Network::Regtest(ActivationHeights::from_zebra_pairs(
                        parameters.activation_heights().iter(),
                    ))
                } else {
                    Network::Testnet
                }
            }
        }
    }
}

impl From<Network> for zebra_chain::parameters::Network {
    fn from(val: Network) -> Self {
        match val {
            Network::Regtest(activation_heights) => zebra_chain::parameters::Network::new_regtest(
                Into::<ConfiguredActivationHeights>::into(activation_heights).into(),
            ),
            Network::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
            Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
        }
    }
}

impl From<&Network> for zebra_chain::parameters::Network {
    fn from(val: &Network) -> Self {
        (*val).into()
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
