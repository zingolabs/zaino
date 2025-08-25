//! TestVectorGenerator test manager for test_vectors.rs integration tests.
//!
//! **Purpose**: Generate test vector data and validate transaction parsing  
//! **Scope**: Validator + StateService + Clients + Test Vector Generation + Transaction Parsing
//! **Use Case**: When creating test data for unit tests and validating transaction parsing functionality
//!
//! This manager provides components and methods specifically designed for the test_vectors.rs
//! integration test suite, which generates test vectors and validates transaction parsing.
//! The name reflects its primary purpose: generating test vector files for consumption by other tests.

use crate::{
    clients::Clients,
    config::{TestVectorTestConfig, TestConfig},
    manager::{
        factories::StateServiceBuilder,
        traits::{ConfigurableBuilder, LaunchManager, WithClients, WithServiceFactories, WithValidator},
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_state::{StateService, StateServiceSubscriber};

/// TestVectorGenerator manager for test_vectors.rs integration tests.
/// 
/// **Purpose**: Generate test vector data and validate transaction parsing
/// **Scope**: 
/// - Validator (Zebra or Zcashd)
/// - StateService with custom configuration for test vector generation
/// - Wallet clients for creating complex transaction scenarios
/// - Network type conversion utilities for test vector compatibility
/// 
/// **Use Case**: When you need to generate test vectors for unit tests and validate
/// transaction parsing functionality across different transaction versions.
/// 
/// **Components**:
/// - Validator: Configurable (Zebra/Zcashd) with extended wait times for real networks
/// - StateService: Custom configured for test vector generation with proper network types
/// - Clients: Always enabled for wallet operations and transaction creation
/// - Network conversion: Supports services::network::Network to zebra::Network conversion
///
/// **Example Usage**:
/// ```rust
/// // Basic test vector generation with regtest
/// let manager = TestVectorGeneratorTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .launch().await?;
/// 
/// let (state_service, subscriber) = manager.create_state_service_for_vectors(None).await?;
/// 
/// // For mainnet/testnet test vector generation
/// let manager = TestVectorGeneratorTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .mainnet()
///     .launch().await?;
/// 
/// let (state_service, subscriber) = manager.create_state_service_for_vectors(Some(cache_dir)).await?;
/// ```
#[derive(Debug)]
pub struct TestVectorGeneratorTestManager {
    /// Local validator network
    pub local_net: LocalNet,
    /// Test ports and directories
    pub ports: TestPorts,
    /// Network configuration
    pub network: Network,
    /// Optional chain cache directory
    pub chain_cache: Option<PathBuf>,
    /// Wallet clients (always enabled for test vectors)
    pub clients: Clients,
}

impl WithValidator for TestVectorGeneratorTestManager {
    fn local_net(&self) -> &LocalNet {
        &self.local_net
    }

    fn local_net_mut(&mut self) -> &mut LocalNet {
        &mut self.local_net
    }

    fn validator_rpc_address(&self) -> SocketAddr {
        self.ports.validator_rpc
    }

    fn validator_grpc_address(&self) -> SocketAddr {
        self.ports.validator_grpc
    }

    fn network(&self) -> &Network {
        &self.network
    }
}

impl WithClients for TestVectorGeneratorTestManager {
    fn clients(&self) -> &Clients {
        &self.clients
    }

    fn clients_mut(&mut self) -> &mut Clients {
        &mut self.clients
    }
}

impl WithServiceFactories for TestVectorGeneratorTestManager {
    fn create_fetch_service(&self) -> crate::manager::factories::FetchServiceBuilder {
        // Test vectors don't typically use FetchService, but provide for completeness
        crate::manager::factories::FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        StateServiceBuilder::new()
            .with_validator_rpc_address(self.validator_rpc_address())
            .with_validator_grpc_address(self.validator_grpc_address())
            .with_network(self.network.clone())
            .with_cache_dir(self.get_chain_cache_dir())
    }

    fn create_json_connector(&self) -> Result<zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector, Box<dyn std::error::Error>> {
        use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?;
        Ok(connector)
    }

    fn create_block_cache(&self) -> crate::manager::factories::BlockCacheBuilder {
        let connector = self
            .create_json_connector()
            .expect("Failed to create connector for block cache");

        crate::manager::factories::BlockCacheBuilder::new(
            connector,
            self.network.clone(),
            self.ports.zaino_db.clone(),
        )
    }
}

impl TestVectorGeneratorTestManager {
    /// Convert zaino Network to services::network::Network for compatibility.
    /// 
    /// This is needed for some legacy APIs that expect the services network type.
    pub fn to_services_network(&self) -> Option<zingo_infra_services::network::Network> {
        match self.network {
            Network::Mainnet => Some(zingo_infra_services::network::Network::Mainnet),
            Network::Testnet => Some(zingo_infra_services::network::Network::Testnet),
            Network::Regtest => None, // Regtest uses None in the original pattern
        }
    }

    /// Convert zaino Network to zebra Network for StateService configuration.
    /// 
    /// This matches the network conversion logic used in test_vectors.rs tests.
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        match self.network {
            Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
            Network::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
            Network::Regtest => zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // TODO: What is network upgrade 6.1? What does a minor version NU mean?
                    nu6_1: None,
                    nu7: None,
                },
            ),
        }
    }

    /// Create StateService configured for test vector generation.
    /// 
    /// This uses the existing StateServiceBuilder infrastructure instead of
    /// trying to recreate the original complex configuration.
    /// 
    /// **Parameters:**
    /// - `custom_cache_dir`: Optional custom cache directory for state storage
    /// 
    /// Returns: (StateService, StateServiceSubscriber)
    pub async fn create_state_service_for_vectors(
        &self,
        custom_cache_dir: Option<PathBuf>,
    ) -> Result<(StateService, StateServiceSubscriber), Box<dyn std::error::Error>> {
        // Wait for validator if using real networks (matches original pattern)
        if matches!(self.network, Network::Mainnet | Network::Testnet) {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
        }

        // Print validator output for debugging (matches original pattern)  
        use crate::validator::Validator as _;
        self.local_net.print_stdout();

        // Use the existing state service builder with custom cache dir if provided
        let mut builder = self.create_state_service();
        if let Some(cache_dir) = custom_cache_dir {
            builder = builder.with_cache_dir(cache_dir);
        }

        // Build the state service (the builder will use default settings)
        let (state_service, state_service_subscriber) = builder
            .build()
            .await?;

        let state_subscriber = state_service_subscriber;

        // Brief settling time (matches original pattern)
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok((state_service, state_subscriber))
    }

    /// Create StateService using the legacy pattern signature.
    /// 
    /// This matches the original `create_test_manager_and_services()` function signature
    /// for backward compatibility.
    /// 
    /// **Parameters:**
    /// - `chain_cache`: Optional chain cache directory
    /// - `enable_zaino`: Whether to enable zaino processing (currently ignored)
    /// - `network`: Optional services network type override
    /// 
    /// Returns: (StateService, StateServiceSubscriber)
    pub async fn create_state_service_legacy_pattern(
        &self,
        chain_cache: Option<PathBuf>,
        _enable_zaino: bool,
        network: Option<zingo_infra_services::network::Network>,
    ) -> Result<(StateService, StateServiceSubscriber), Box<dyn std::error::Error>> {
        // If network override is provided, we should use it, but for now we'll just use the configured network
        // TODO: Consider supporting network override in the future if needed
        let _network_override = network;
        
        self.create_state_service_for_vectors(chain_cache).await
    }

    /// Get the chain cache directory, using the configured one or default.
    pub fn get_chain_cache_dir(&self) -> PathBuf {
        self.chain_cache.clone().unwrap_or_else(|| self.ports.zaino_db.clone())
    }

    /// Common test pattern: Generate blocks and setup for test vector creation.
    /// 
    /// This combines the pattern seen in test_vectors.rs tests for preparing
    /// the blockchain state for test vector generation.
    /// 
    /// **Parameters:**
    /// - `initial_blocks`: Number of initial blocks to mine (typically 100 for coinbase maturity)
    /// - `setup_wallets`: Whether to sync wallets and prepare for transactions
    pub async fn prepare_for_vector_generation(
        &mut self,
        initial_blocks: u32,
        setup_wallets: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Mine initial blocks to finalize first block reward (matches original pattern)
        println!("Mining {} initial blocks to finalize block rewards...", initial_blocks);
        self.generate_blocks_with_delay(initial_blocks).await?;
        
        if setup_wallets {
            // Sync wallets after initial mining
            self.faucet().sync_and_await().await?;
            
            // Brief settling time
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        
        Ok(())
    }

    /// Execute a complex transaction creation and mining scenario.
    /// 
    /// This provides the transaction creation patterns seen in test_vectors.rs
    /// for building chains with various transaction types.
    /// 
    /// **Parameters:**
    /// - `operations`: Vector of operation types to perform
    /// - `recipient_addresses`: Tuple of (transparent, sapling, unified) addresses for sending
    pub async fn execute_transaction_scenario(
        &mut self,
        operations: Vec<TransactionOperation>,
        recipient_addresses: Option<(String, String, String)>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for operation in operations {
            match operation {
                TransactionOperation::Shield => {
                    self.faucet().quick_shield().await?;
                }
                TransactionOperation::SendToUnified(amount) => {
                    if let Some((_, _, uaddr)) = &recipient_addresses {
                        crate::from_inputs::quick_send(
                            self.faucet(),
                            vec![(uaddr.as_str(), amount, None)],
                        )
                        .await?;
                    }
                }
                TransactionOperation::SendToTransparent(amount) => {
                    if let Some((taddr, _, _)) = &recipient_addresses {
                        crate::from_inputs::quick_send(
                            self.faucet(),
                            vec![(taddr.as_str(), amount, None)],
                        )
                        .await?;
                    }
                }
                TransactionOperation::SendToSapling(amount) => {
                    if let Some((_, saddr, _)) = &recipient_addresses {
                        crate::from_inputs::quick_send(
                            self.faucet(),
                            vec![(saddr.as_str(), amount, None)],
                        )
                        .await?;
                    }
                }
                TransactionOperation::MineBlock => {
                    self.generate_blocks(1).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                TransactionOperation::SyncWallets => {
                    self.faucet().sync_and_await().await?;
                    self.recipient().sync_and_await().await?;
                }
            }
        }
        
        Ok(())
    }

    /// Get all address types for both faucet and recipient wallets.
    /// 
    /// This provides the address extraction pattern used in test_vectors.rs tests.
    /// 
    /// Returns: ((faucet_taddr, faucet_saddr, faucet_uaddr), (recipient_taddr, recipient_saddr, recipient_uaddr))
    pub async fn get_all_addresses(&mut self) -> Result<((String, String, String), (String, String, String)), Box<dyn std::error::Error>> {
        use crate::clients::ClientAddressType;
        
        let faucet_taddr = self.get_faucet_address(ClientAddressType::Transparent).await;
        let faucet_saddr = self.get_faucet_address(ClientAddressType::Sapling).await;
        let faucet_uaddr = self.get_faucet_address(ClientAddressType::Unified).await;

        let recipient_taddr = self.get_recipient_address(ClientAddressType::Transparent).await;
        let recipient_saddr = self.get_recipient_address(ClientAddressType::Sapling).await;
        let recipient_uaddr = self.get_recipient_address(ClientAddressType::Unified).await;

        Ok((
            (faucet_taddr, faucet_saddr, faucet_uaddr),
            (recipient_taddr, recipient_saddr, recipient_uaddr),
        ))
    }

    /// Validate raw transaction parsing.
    /// 
    /// This provides basic transaction parsing validation functionality.
    /// For full test vector validation, use the integration-tests directly.
    pub fn validate_raw_transaction_parsing(
        &self,
        raw_tx: &[u8],
        expected_version: u32,
        txid_hint: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use zaino_fetch::chain::transaction::FullTransaction;
        use zaino_fetch::chain::utils::ParseFromSlice;
        
        let txid_vec = txid_hint.map(|tx| vec![tx]);
        let deserialized_tx = FullTransaction::parse_from_slice(
            raw_tx,
            txid_vec,
            None,
        )
        .map_err(|e| format!("Failed to deserialize transaction: {e}"))?;

        let tx = deserialized_tx.1;

        if tx.version() != expected_version {
            return Err(format!(
                "Version mismatch: expected {expected_version}, got {}",
                tx.version()
            ).into());
        }

        println!("✓ Transaction parsed correctly with version {}", tx.version());
        Ok(())
    }
}

/// Operations that can be performed during transaction scenario execution.
#[derive(Debug, Clone)]
pub enum TransactionOperation {
    /// Shield transparent funds to shielded pool
    Shield,
    /// Send to a unified address with specified amount
    SendToUnified(u64),
    /// Send to a transparent address with specified amount  
    SendToTransparent(u64),
    /// Send to a sapling address with specified amount
    SendToSapling(u64),
    /// Mine a single block
    MineBlock,
    /// Sync both faucet and recipient wallets
    SyncWallets,
}

/// Builder for TestVectorGeneratorTestManager.
#[derive(Debug, Clone)]
pub struct TestVectorGeneratorTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
}

impl Default for TestVectorGeneratorTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
        }
    }
}

impl ConfigurableBuilder for TestVectorGeneratorTestsBuilder {
    type Manager = TestVectorGeneratorTestManager;
    type Config = TestVectorTestConfig;

    fn build_config(&self) -> Self::Config {
        TestVectorTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        let config = self.build_config();
        config.launch_manager().await
    }

    fn validator(mut self, kind: ValidatorKind) -> Self {
        self.validator_kind = kind;
        self
    }

    fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    fn chain_cache(mut self, path: PathBuf) -> Self {
        self.chain_cache = Some(path);
        self
    }
}

impl TestVectorGeneratorTestsBuilder {
    /// Configure for Zcashd validator.
    pub fn zcashd(mut self) -> Self {
        self.validator_kind = ValidatorKind::Zcashd;
        self
    }

    /// Configure for Zebra validator.  
    pub fn zebra(mut self) -> Self {
        self.validator_kind = ValidatorKind::Zebra;
        self
    }

    /// Configure for mainnet testing.
    pub fn mainnet(mut self) -> Self {
        self.network = Network::Mainnet;
        self
    }

    /// Configure for testnet testing.
    pub fn testnet(mut self) -> Self {
        self.network = Network::Testnet;
        self
    }

    /// Configure for regtest (default).
    pub fn regtest(mut self) -> Self {
        self.network = Network::Regtest;
        self
    }
}