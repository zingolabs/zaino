//! Holds config data for Zaino-State services.

use std::path::PathBuf;
use zaino_common::{Network, ServiceConfig, StorageConfig};
use zcash_address::ZcashAddress;

/// A validated Zcash donation address (transparent, sapling, orchard, or unified).
///
/// Constructed only from a string that parses as a valid [`ZcashAddress`], so
/// the type can never hold an arbitrary or malformed value.
#[derive(Clone, Debug)]
pub struct DonationAddress(ZcashAddress);

impl DonationAddress {
    /// Attempts to parse the given string as a validated Zcash donation address.
    pub(crate) fn try_from_encoded(s: &str) -> Result<Self, zcash_address::ParseError> {
        ZcashAddress::try_from_encoded(s).map(DonationAddress)
    }

    /// Returns the canonical encoded string for this address.
    pub(crate) fn encode(&self) -> String {
        self.0.encode()
    }
}

impl<'de> serde::Deserialize<'de> for DonationAddress {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_from_encoded(&s).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for DonationAddress {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.encode())
    }
}

impl std::fmt::Display for DonationAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.encode())
    }
}

/// Type of backend to be used.
///
/// Determines how Zaino fetches blockchain data from the validator.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    /// Uses Zebra's ReadStateService for direct state access.
    ///
    /// More efficient but requires running on the same machine as Zebra.
    State,
    /// Uses JSON-RPC client to fetch data.
    ///
    /// Compatible with Zcashd, Zebra, or another Zaino instance.
    #[default]
    Fetch,
}

/// Unified backend configuration enum.
#[derive(Debug, Clone)]
#[allow(deprecated)]
pub enum BackendConfig {
    /// StateService config.
    State(StateServiceConfig),
    /// Fetchservice config.
    Fetch(FetchServiceConfig),
}

/// Configuration shared by every backend variant.
///
/// Carries the validator-RPC connection bits plus the runtime indexer
/// settings that are independent of how blockchain data is fetched.
#[derive(Debug, Clone)]
pub struct CommonBackendConfig {
    /// Validator JsonRPC address (supports hostname:port or ip:port format).
    pub validator_rpc_address: String,
    /// Enable validator rpc cookie authentication with Some: path to the validator cookie file.
    pub validator_cookie_path: Option<PathBuf>,
    /// Validator JsonRPC user.
    pub validator_rpc_user: String,
    /// Validator JsonRPC password.
    pub validator_rpc_password: String,
    /// Service-level configuration (timeout, channel size)
    pub service: ServiceConfig,
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Network type.
    pub network: Network,
    /// Zcash donation UA address
    pub donation_address: Option<DonationAddress>,
    /// Version of the indexer binary embedding this service.
    ///
    /// Reported on the wire via `LightdInfo.version`. Defaults to this
    /// crate's `CARGO_PKG_VERSION` when constructed via the parent
    /// service's `new`; the embedding binary should overwrite it with
    /// its own `CARGO_PKG_VERSION` so the wire reflects the deployed
    /// indexer rather than the library crate.
    pub indexer_version: String,
}

/// Holds config data for [crate::StateService].
#[derive(Debug, Clone)]
// #[deprecated]
pub struct StateServiceConfig {
    /// Settings shared with [`FetchServiceConfig`].
    pub common: CommonBackendConfig,
    /// Zebra [`zebra_state::ReadStateService`] config data
    pub validator_state_config: zebra_state::Config,
    /// Validator gRPC address (requires ip:port format for Zebra state sync).
    pub validator_grpc_address: std::net::SocketAddr,
    /// Validator cookie auth.
    pub validator_cookie_auth: bool,
}

#[allow(deprecated)]
impl StateServiceConfig {
    /// Returns a new instance of [`StateServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_state_config: zebra_state::Config,
        validator_rpc_address: String,
        validator_grpc_address: std::net::SocketAddr,
        validator_cookie_auth: bool,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        network: Network,
        donation_address: Option<DonationAddress>,
    ) -> Self {
        tracing::trace!(
            "State service expecting NU activations:\n{:?}",
            network.to_zebra_network().full_activation_list()
        );
        StateServiceConfig {
            common: CommonBackendConfig {
                validator_rpc_address,
                validator_cookie_path,
                validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
                validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
                service,
                storage,
                network,
                donation_address,
                indexer_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            validator_state_config,
            validator_grpc_address,
            validator_cookie_auth,
        }
    }
}

/// Holds config data for [crate::FetchService].
#[derive(Debug, Clone)]
#[deprecated]
pub struct FetchServiceConfig {
    /// Settings shared with [`StateServiceConfig`].
    pub common: CommonBackendConfig,
}

#[allow(deprecated)]
impl FetchServiceConfig {
    /// Returns a new instance of [`FetchServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_rpc_address: String,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        network: Network,
        donation_address: Option<DonationAddress>,
    ) -> Self {
        FetchServiceConfig {
            common: CommonBackendConfig {
                validator_rpc_address,
                validator_cookie_path,
                validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
                validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
                service,
                storage,
                network,
                donation_address,
                indexer_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    }
}

/// Holds config data for `[ZainoDb]`.
/// TODO: Rename  to *ZainoDbConfig* when ChainIndex update is complete **and** remove legacy fields.
#[derive(Debug, Clone)]
pub struct BlockCacheConfig {
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Database version selected to be run.
    pub db_version: u32,
    /// Network type.
    pub network: Network,
}

impl BlockCacheConfig {
    /// Returns a new instance of [`BlockCacheConfig`].
    #[allow(dead_code)]
    pub fn new(storage: StorageConfig, db_version: u32, network: Network, _no_sync: bool) -> Self {
        BlockCacheConfig {
            storage,
            db_version,
            network,
        }
    }
}

impl From<CommonBackendConfig> for BlockCacheConfig {
    fn from(value: CommonBackendConfig) -> Self {
        Self {
            storage: value.storage,
            // TODO: update zaino configs to include db version.
            db_version: 1,
            network: value.network,
        }
    }
}

#[allow(deprecated)]
impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(value: StateServiceConfig) -> Self {
        value.common.into()
    }
}

#[allow(deprecated)]
impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(value: FetchServiceConfig) -> Self {
        value.common.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod donation_address {
        use super::*;
        use zcash_address::{unified::Encoding as _, ToAddress as _, ZcashAddress};
        use zcash_protocol::consensus::NetworkType;

        // --- valid addresses ---

        #[test]
        fn valid_transparent_p2pkh() {
            let encoded =
                ZcashAddress::from_transparent_p2pkh(NetworkType::Main, [1u8; 20]).encode();
            assert!(DonationAddress::try_from_encoded(&encoded).is_ok());
        }

        #[test]
        fn valid_transparent_p2sh() {
            let encoded =
                ZcashAddress::from_transparent_p2sh(NetworkType::Main, [2u8; 20]).encode();
            assert!(DonationAddress::try_from_encoded(&encoded).is_ok());
        }

        #[test]
        fn valid_sapling() {
            let encoded = ZcashAddress::from_sapling(NetworkType::Main, [3u8; 43]).encode();
            assert!(DonationAddress::try_from_encoded(&encoded).is_ok());
        }

        #[test]
        fn valid_unified_orchard() {
            let (_network, ua) = zcash_address::unified::Address::decode(
            "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666kgwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw6uh7hyvzjse8ksx"
        ).unwrap();
            let encoded = ZcashAddress::from_unified(NetworkType::Main, ua).encode();
            assert!(DonationAddress::try_from_encoded(&encoded).is_ok());
        }

        // --- invalid addresses ---

        #[test]
        fn invalid_empty_string() {
            assert!(DonationAddress::try_from_encoded("").is_err());
        }

        #[test]
        fn invalid_arbitrary_text() {
            assert!(DonationAddress::try_from_encoded("not_a_zcash_address").is_err());
        }

        #[test]
        fn invalid_truncated_prefix() {
            assert!(DonationAddress::try_from_encoded("t1abc").is_err());
        }

        // --- round-trip ---

        #[test]
        fn round_trip_transparent() {
            let encoded =
                ZcashAddress::from_transparent_p2pkh(NetworkType::Main, [5u8; 20]).encode();
            assert_eq!(
                DonationAddress::try_from_encoded(&encoded)
                    .unwrap()
                    .encode(),
                encoded
            );
        }

        #[test]
        fn round_trip_sapling() {
            let encoded = ZcashAddress::from_sapling(NetworkType::Main, [6u8; 43]).encode();
            assert_eq!(
                DonationAddress::try_from_encoded(&encoded)
                    .unwrap()
                    .encode(),
                encoded
            );
        }

        #[test]
        fn round_trip_unified() {
            let (_network, ua) = zcash_address::unified::Address::decode(
            "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666kgwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw6uh7hyvzjse8ksx"
        ).unwrap();

            let encoded = ZcashAddress::from_unified(NetworkType::Main, ua).encode();
            assert_eq!(
                DonationAddress::try_from_encoded(&encoded)
                    .unwrap()
                    .encode(),
                encoded
            );
        }
    }
}
