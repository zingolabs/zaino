//! Network type for Zaino configuration.

use serde::{Deserialize, Serialize};
use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

pub const ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS: ActivationHeights = ActivationHeights {
    overwinter: Some(1),
    before_overwinter: Some(1),
    sapling: Some(1),
    blossom: Some(1),
    heartwood: Some(1),
    canopy: Some(1),
    nu5: Some(2),
    nu6: Some(2),
    nu6_1: Some(1000),
    nu7: None,
};

/// Network type for Zaino configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(from = "NetworkDeserialize")]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// Testnet network
    Testnet,
    /// Regtest network (for local testing)
    Regtest(ActivationHeights),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum NetworkDeserialize {
    Mainnet,
    Testnet,
    // This hack doesn't allow deserializing
    // non-default ActivationHeights, this
    // will need to be revisited if we want
    // to support that in read configs
    Regtest,
}

impl From<NetworkDeserialize> for Network {
    fn from(value: NetworkDeserialize) -> Self {
        match value {
            NetworkDeserialize::Mainnet => Network::Mainnet,
            NetworkDeserialize::Testnet => Network::Testnet,
            NetworkDeserialize::Regtest => Network::Regtest(ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS),
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
            nu7: None,
        }
    }
}

impl From<ConfiguredActivationHeights> for ActivationHeights {
    fn from(
        ConfiguredActivationHeights {
            before_overwinter,
            overwinter,
            sapling,
            blossom,
            heartwood,
            canopy,
            nu5,
            nu6,
            nu6_1,
            nu7,
        }: ConfiguredActivationHeights,
    ) -> Self {
        Self {
            before_overwinter,
            overwinter,
            sapling,
            blossom,
            heartwood,
            canopy,
            nu5,
            nu6,
            nu6_1,
            nu7,
        }
    }
}
impl From<ActivationHeights> for ConfiguredActivationHeights {
    fn from(
        ActivationHeights {
            before_overwinter,
            overwinter,
            sapling,
            blossom,
            heartwood,
            canopy,
            nu5,
            nu6,
            nu6_1,
            nu7,
        }: ActivationHeights,
    ) -> Self {
        Self {
            before_overwinter,
            overwinter,
            sapling,
            blossom,
            heartwood,
            canopy,
            nu5,
            nu6,
            nu6_1,
            nu7,
        }
    }
}

impl Network {
    /// Convert to Zebra's network type using default configurations.
    pub fn to_zebra_default(&self) -> zebra_chain::parameters::Network {
        self.into()
    }

    /// Convert to Zebra's network type for internal use (alias for to_zebra_default).
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        self.to_zebra_default()
    }

    /// Get the standard regtest activation heights used by Zaino.
    pub fn zaino_regtest_heights() -> ConfiguredActivationHeights {
        ConfiguredActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(1),
            nu6: Some(1),
            nu6_1: None,
            nu7: None,
        }
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
                    let mut activation_heights = ActivationHeights {
                        before_overwinter: None,
                        overwinter: None,
                        sapling: None,
                        blossom: None,
                        heartwood: None,
                        canopy: None,
                        nu5: None,
                        nu6: None,
                        nu6_1: None,
                        nu7: None,
                    };
                    for (height, upgrade) in parameters.activation_heights().iter() {
                        match upgrade {
                            zebra_chain::parameters::NetworkUpgrade::Genesis => (),
                            zebra_chain::parameters::NetworkUpgrade::BeforeOverwinter => {
                                activation_heights.before_overwinter = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Overwinter => {
                                activation_heights.overwinter = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Sapling => {
                                activation_heights.sapling = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Blossom => {
                                activation_heights.blossom = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Heartwood => {
                                activation_heights.heartwood = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Canopy => {
                                activation_heights.canopy = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Nu5 => {
                                activation_heights.nu5 = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Nu6 => {
                                activation_heights.nu6 = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Nu6_1 => {
                                activation_heights.nu6_1 = Some(height.0)
                            }
                            zebra_chain::parameters::NetworkUpgrade::Nu7 => {
                                activation_heights.nu7 = Some(height.0)
                            }
                        }
                    }
                    Network::Regtest(activation_heights)
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
            Network::Regtest(activation_heights) => {
                zebra_chain::parameters::Network::new_regtest(activation_heights.into())
            }
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
