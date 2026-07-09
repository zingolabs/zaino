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

/// How the [`NodeBackedIndexerService`](crate::NodeBackedIndexerService) connects to its
/// validator to source blockchain data.
///
/// Carries the connection-specific configuration inline: the `Direct` variant owns the
/// Zebra `ReadStateService` settings, while `Rpc` needs only the JSON-RPC connection bits
/// already held in [`CommonBackendConfig`].
#[derive(Debug, Clone)]
pub enum ValidatorConnectionType {
    /// JSON-RPC connection (formerly `Fetch`).
    ///
    /// Compatible with Zcashd, Zebra, or another Zaino instance.
    Rpc,
    /// Direct Zebra `ReadStateService` connection (formerly `State`).
    ///
    /// More efficient but requires running alongside a Zebra whose state DB and gRPC
    /// sync endpoint we own.
    Direct(DirectConnectionConfig),
}

/// Connection parameters for the [`ValidatorConnectionType::Direct`] backend.
///
/// Bundles the Zebra `ReadStateService` DB config with the gRPC sync endpoint used to
/// drive it, so the whole "direct connection" description travels as one value.
#[derive(Debug, Clone)]
pub struct DirectConnectionConfig {
    /// Zebra [`zebra_state::ReadStateService`] config data (DB cache dir etc.).
    pub validator_state_config: zebra_state::Config,
    /// Validator gRPC address (requires ip:port format for Zebra state sync).
    pub validator_grpc_address: std::net::SocketAddr,
    /// Whether validator cookie authentication is enabled.
    pub validator_cookie_auth: bool,
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
    /// Ephemeral finalised state:
    ///
    /// If true, FinalisedState does not write data to disk,
    /// fetching data  from the backing validator.
    ///
    /// Note that full functionality is not available and
    /// performanc will be reduced in this configuration.
    pub ephemeral_finalised_state: bool,
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

impl CommonBackendConfig {
    /// Builds a [`CommonBackendConfig`], applying the default RPC user/password
    /// placeholder (`"xxxxxx"`) and this crate's `CARGO_PKG_VERSION` as the
    /// indexer version. Shared by the [`NodeBackedIndexerServiceConfig`] constructors.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_rpc_address: String,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        ephemeral_finalised_state: bool,
        network: Network,
        donation_address: Option<DonationAddress>,
    ) -> Self {
        CommonBackendConfig {
            validator_rpc_address,
            validator_cookie_path,
            validator_rpc_user: validator_rpc_user.unwrap_or_else(|| "xxxxxx".to_string()),
            validator_rpc_password: validator_rpc_password.unwrap_or_else(|| "xxxxxx".to_string()),
            service,
            storage,
            ephemeral_finalised_state,
            network,
            donation_address,
            indexer_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Holds config data for [`crate::NodeBackedIndexerService`].
///
/// Replaces the former per-backend `FetchServiceConfig` / `StateServiceConfig`: the
/// shared bits live in [`CommonBackendConfig`] and the backend-specific bits (only the
/// `Direct` backend has any) live in [`ValidatorConnectionType`].
#[derive(Debug, Clone)]
pub struct NodeBackedIndexerServiceConfig {
    /// Connection-independent settings (validator RPC, storage, network, ...).
    pub common: CommonBackendConfig,
    /// Which validator connection to use, and its connection-specific config.
    pub connection: ValidatorConnectionType,
}

impl NodeBackedIndexerServiceConfig {
    /// Returns a JSON-RPC (`Rpc`) service config (formerly `FetchServiceConfig::new`).
    #[allow(clippy::too_many_arguments)]
    pub fn new_rpc(
        validator_rpc_address: String,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        ephemeral_finalised_state: bool,
        network: Network,
        donation_address: Option<DonationAddress>,
    ) -> Self {
        NodeBackedIndexerServiceConfig {
            common: CommonBackendConfig::new(
                validator_rpc_address,
                validator_cookie_path,
                validator_rpc_user,
                validator_rpc_password,
                service,
                storage,
                ephemeral_finalised_state,
                network,
                donation_address,
            ),
            connection: ValidatorConnectionType::Rpc,
        }
    }

    /// Returns a direct-`ReadStateService` (`Direct`) service config
    /// (formerly `StateServiceConfig::new`).
    #[allow(clippy::too_many_arguments)]
    pub fn new_direct(
        validator_state_config: zebra_state::Config,
        validator_rpc_address: String,
        validator_grpc_address: std::net::SocketAddr,
        validator_cookie_auth: bool,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        ephemeral_finalised_state: bool,
        network: Network,
        donation_address: Option<DonationAddress>,
    ) -> Self {
        // The config carries only the network kind; the activation schedule
        // is adopted from the validator at spawn and logged there (#1076).
        NodeBackedIndexerServiceConfig {
            common: CommonBackendConfig::new(
                validator_rpc_address,
                validator_cookie_path,
                validator_rpc_user,
                validator_rpc_password,
                service,
                storage,
                ephemeral_finalised_state,
                network,
                donation_address,
            ),
            connection: ValidatorConnectionType::Direct(DirectConnectionConfig {
                validator_state_config,
                validator_grpc_address,
                validator_cookie_auth,
            }),
        }
    }
}

/// Holds config data for `[ChainIndex]` and sub-components.
#[derive(Debug, Clone)]
pub struct ChainIndexConfig {
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Database version selected to be run.
    pub db_version: u32,
    /// The runtime network, carrying the activation schedule adopted from
    /// the validator (zaino#1076) — or, in fixtures that are their own
    /// chain, the fixture's schedule.
    pub network: zebra_chain::parameters::Network,
    /// Ephemeral finalised state:
    ///
    /// If true, FinalisedState does not write data to disk,
    /// fetching data  from the backing validator.
    ///
    /// Note that full functionality is not available and
    /// performanc will be reduced in this configuration.
    pub ephemeral: bool,
}

impl ChainIndexConfig {
    /// Returns a new instance of [`ChainIndexConfig`].
    #[allow(dead_code)]
    pub fn new(
        storage: StorageConfig,
        db_version: u32,
        network: zebra_chain::parameters::Network,
        ephemeral: bool,
    ) -> Self {
        ChainIndexConfig {
            storage,
            db_version,
            network,
            ephemeral,
        }
    }

    /// Builds the chain-index config from the backend config plus the
    /// runtime network. The backend config carries only a network kind, so
    /// the adopted runtime network arrives as its own argument — there is
    /// no conversion from a service config alone.
    pub fn from_backend_config(
        common: &CommonBackendConfig,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        Self {
            storage: common.storage.clone(),
            // TODO: update zaino configs to include db version.
            db_version: 1,
            network,
            ephemeral: common.ephemeral_finalised_state,
        }
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
