//! Validator launching and management.

use std::path::PathBuf;
use tempfile::TempDir;
use testvectors::REG_O_ADDR_FROM_ABANDONART;
use zingo_infra_services::network::Network as InfraNetwork;
pub use zingo_infra_services::validator::Validator;

use crate::{
    binaries::*,
    environment::{TestEnvironment, ValidatorKind},
    ports::TestPorts,
};

/// Config for validators.
pub enum ValidatorConfig {
    /// Zcashd Config.
    ZcashdConfig(zingo_infra_services::validator::ZcashdConfig),
    /// Zebrd Config.
    ZebrdConfig(zingo_infra_services::validator::ZebradConfig),
}

/// Available zcash-local-net configurations.
#[allow(
    clippy::large_enum_variant,
    reason = "Maybe this issue: https://github.com/rust-lang/rust-clippy/issues/9798"
)]
pub enum LocalNet {
    /// Zcash-local-net backed by Zcashd.
    Zcashd(
        zingo_infra_services::LocalNet<
            zingo_infra_services::indexer::Empty,
            zingo_infra_services::validator::Zcashd,
        >,
    ),
    /// Zcash-local-net backed by Zebrd.
    Zebrd(
        zingo_infra_services::LocalNet<
            zingo_infra_services::indexer::Empty,
            zingo_infra_services::validator::Zebrad,
        >,
    ),
}

impl zingo_infra_services::validator::Validator for LocalNet {
    const CONFIG_FILENAME: &str = "";

    type Config = ValidatorConfig;

    fn activation_heights(&self) -> zingo_infra_services::network::ActivationHeights {
        match self {
            LocalNet::Zcashd(net) => net.validator().activation_heights(),
            LocalNet::Zebrd(net) => net.validator().activation_heights(),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn launch(
        config: Self::Config,
    ) -> impl std::future::Future<Output = Result<Self, zingo_infra_services::error::LaunchError>> + Send
    {
        async move {
            match config {
                ValidatorConfig::ZcashdConfig(cfg) => {
                    let net = zingo_infra_services::LocalNet::<
                        zingo_infra_services::indexer::Empty,
                        zingo_infra_services::validator::Zcashd,
                    >::launch(
                        zingo_infra_services::indexer::EmptyConfig {}, cfg
                    )
                    .await;
                    Ok(LocalNet::Zcashd(net))
                }
                ValidatorConfig::ZebrdConfig(cfg) => {
                    let net = zingo_infra_services::LocalNet::<
                        zingo_infra_services::indexer::Empty,
                        zingo_infra_services::validator::Zebrad,
                    >::launch(
                        zingo_infra_services::indexer::EmptyConfig {}, cfg
                    )
                    .await;
                    Ok(LocalNet::Zebrd(net))
                }
            }
        }
    }

    fn stop(&mut self) {
        match self {
            LocalNet::Zcashd(net) => net.validator_mut().stop(),
            LocalNet::Zebrd(net) => net.validator_mut().stop(),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn generate_blocks(
        &self,
        n: u32,
    ) -> impl std::future::Future<Output = std::io::Result<()>> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().generate_blocks(n).await,
                LocalNet::Zebrd(net) => net.validator().generate_blocks(n).await,
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn get_chain_height(
        &self,
    ) -> impl std::future::Future<Output = zcash_protocol::consensus::BlockHeight> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().get_chain_height().await,
                LocalNet::Zebrd(net) => net.validator().get_chain_height().await,
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn poll_chain_height(
        &self,
        target_height: zcash_protocol::consensus::BlockHeight,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().poll_chain_height(target_height).await,
                LocalNet::Zebrd(net) => net.validator().poll_chain_height(target_height).await,
            }
        }
    }

    fn config_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().config_dir(),
            LocalNet::Zebrd(net) => net.validator().config_dir(),
        }
    }

    fn logs_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().logs_dir(),
            LocalNet::Zebrd(net) => net.validator().logs_dir(),
        }
    }

    fn data_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().data_dir(),
            LocalNet::Zebrd(net) => net.validator().data_dir(),
        }
    }

    fn network(&self) -> InfraNetwork {
        match self {
            LocalNet::Zcashd(net) => net.validator().network(),
            LocalNet::Zebrd(net) => *net.validator().network(),
        }
    }

    /// Prints the stdout log.
    fn print_stdout(&self) {
        match self {
            LocalNet::Zcashd(net) => net.validator().print_stdout(),
            LocalNet::Zebrd(net) => net.validator().print_stdout(),
        }
    }

    /// Chain_Cache PathBuf must contain validator bin name for this function to function.
    fn load_chain(
        chain_cache: PathBuf,
        validator_data_dir: PathBuf,
        validator_network: InfraNetwork,
    ) -> PathBuf {
        if chain_cache.to_string_lossy().contains("zcashd") {
            zingo_infra_services::validator::Zcashd::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network,
            )
        } else if chain_cache.to_string_lossy().contains("zebrd") {
            zingo_infra_services::validator::Zebrad::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network,
            )
        } else {
            panic!(
                "Invalid chain_cache path: expected to contain 'zcashd' or 'zebrd', but got: {}",
                chain_cache.display()
            );
        }
    }
}

impl LocalNet {
    /// Launch validator from test environment specification.
    pub async fn launch_from_env(
        env: &TestEnvironment,
        ports: &TestPorts,
    ) -> Result<Self, std::io::Error> {
        let validator_config = match env.validator.kind {
            ValidatorKind::Zcashd => {
                let cfg = zingo_infra_services::validator::ZcashdConfig {
                    zcashd_bin: ZCASHD_BIN.clone(),
                    zcash_cli_bin: ZCASH_CLI_BIN.clone(),
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: Some(REG_O_ADDR_FROM_ABANDONART),
                    chain_cache: env.validator.chain_cache.clone(),
                };
                ValidatorConfig::ZcashdConfig(cfg)
            }
            ValidatorKind::Zebrd => {
                let cfg = zingo_infra_services::validator::ZebradConfig {
                    zebrad_bin: ZEBRD_BIN.clone(),
                    network_listen_port: None,
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: zingo_infra_services::validator::ZEBRAD_DEFAULT_MINER,
                    chain_cache: env.validator.chain_cache.clone(),
                    network: env.validator.network.into(),
                };
                ValidatorConfig::ZebrdConfig(cfg)
            }
        };

        Self::launch(validator_config)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to launch validator: {}", e)))
    }
}
