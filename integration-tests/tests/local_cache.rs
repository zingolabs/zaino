use anyhow::{Context, Result};
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_state::bench::{BlockCache, BlockCacheConfig, BlockCacheSubscriber};
use zaino_testutils::Validator as _;
use zaino_testutils::{TestConfigBuilder, TestManager};
use zebra_chain::block::Height;
use zebra_state::HashOrHeight;

/// Validator type for test configuration
#[derive(Debug, Clone, Copy)]
enum ValidatorType {
    Zcashd,
    Zebra,
}

/// Configuration for local cache tests
#[derive(Debug, Clone)]
struct LocalCacheTestConfig {
    validator_type: ValidatorType,
    enable_db: bool,
    enable_sync: bool,
    batch_count: u32,
    batch_size: u32,
}

impl Default for LocalCacheTestConfig {
    fn default() -> Self {
        Self {
            validator_type: ValidatorType::Zebra,
            enable_db: true,
            enable_sync: true,
            batch_count: 1,
            batch_size: 100,
        }
    }
}

impl LocalCacheTestConfig {
    fn for_zcashd() -> Self {
        Self {
            validator_type: ValidatorType::Zcashd,
            ..Default::default()
        }
    }

    fn for_zebra() -> Self {
        Self {
            validator_type: ValidatorType::Zebra,
            ..Default::default()
        }
    }

    fn without_db(mut self) -> Self {
        self.enable_db = false;
        self
    }

    fn without_sync(mut self) -> Self {
        self.enable_sync = false;
        self
    }

    fn with_batches(mut self, count: u32) -> Self {
        self.batch_count = count;
        self
    }

    fn with_batch_size(mut self, size: u32) -> Self {
        self.batch_size = size;
        self
    }
}

/// Complete test environment for local cache testing
struct LocalCacheTestEnvironment {
    test_manager: TestManager,
    json_service: JsonRpSeeConnector,
    block_cache: BlockCache,
    block_cache_subscriber: BlockCacheSubscriber,
}

impl LocalCacheTestEnvironment {
    /// Create a new test environment from configuration
    async fn new(config: &LocalCacheTestConfig) -> Result<Self> {
        let test_manager = Self::create_test_manager(&config).await?;
        let json_service = Self::create_json_service(&test_manager).await?;
        let block_cache = Self::create_block_cache(&test_manager, &json_service, &config).await?;
        let block_cache_subscriber = block_cache.subscriber();

        Ok(Self {
            test_manager,
            json_service,
            block_cache,
            block_cache_subscriber,
        })
    }

    /// Create TestManager with appropriate configuration
    async fn create_test_manager(config: &LocalCacheTestConfig) -> Result<TestManager> {
        let mut builder = match config.validator_type {
            ValidatorType::Zcashd => TestConfigBuilder::remote_zcashd(),
            ValidatorType::Zebra => TestConfigBuilder::remote_zebra(),
        };

        // Apply configuration flags
        if config.enable_sync && config.enable_db {
            builder = builder.with_sync_and_db();
        }

        TestManager::launch(builder)
            .await
            .context("Failed to launch test manager")
    }

    /// Create JSON RPC service connector
    async fn create_json_service(test_manager: &TestManager) -> Result<JsonRpSeeConnector> {
        JsonRpSeeConnector::from_backend_config(&test_manager.config.backend)
            .await
            .context("Failed to create JSON RPC connector from backend config")
    }

    /// Create block cache with appropriate configuration
    async fn create_block_cache(
        test_manager: &TestManager,
        json_service: &JsonRpSeeConnector,
        config: &LocalCacheTestConfig,
    ) -> Result<BlockCache> {
        // Construct BlockCacheConfig directly from IndexerConfig fields
        let block_cache_config = BlockCacheConfig {
            cache: test_manager.config.storage.cache.clone(),
            database: test_manager.config.storage.database.clone(),
            network: test_manager.config.network,
            // todo!: this smells off... let's investigate the proper source of these flags...
            no_sync: !config.enable_sync || test_manager.config.debug.no_sync,
            no_db: !config.enable_db || test_manager.config.debug.no_db,
        };

        BlockCache::spawn(json_service, None, block_cache_config)
            .await
            .context("Failed to spawn block cache")
    }

    /// Generate blocks in batches with appropriate delays
    async fn generate_blocks(&self, count: u32) -> Result<()> {
        for height in 1..=count {
            println!("Generating block at height: {height}");
            self.test_manager
                .local_net
                .generate_blocks(1)
                .await
                .with_context(|| format!("Failed to generate block at height {height}"))?;

            // Small delay to allow other tasks to run (especially important for zcashd)
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// Wait for synchronization with a reasonable timeout
    async fn wait_for_sync(&self) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        Ok(())
    }

    /// Get current heights from all components
    async fn get_heights(&self) -> Result<(u32, u32, u32)> {
        let validator_height = self
            .json_service
            .get_blockchain_info()
            .await
            .context("Failed to get validator blockchain info")?
            .blocks
            .0;

        let non_finalised_height = self
            .block_cache_subscriber
            .get_chain_height()
            .await
            .context("Failed to get non-finalised state height")?
            .0;

        let finalised_height = self
            .block_cache
            .finalised_state
            .as_ref()
            .and_then(|fs| fs.get_db_height().ok())
            .unwrap_or(Height(0))
            .0;

        Ok((validator_height, non_finalised_height, finalised_height))
    }
}

/// Test basic cache launch and status checking
async fn test_basic_cache_launch(config: LocalCacheTestConfig) -> Result<()> {
    let env = LocalCacheTestEnvironment::new(&config).await?;

    // Basic status check
    let status = env.block_cache_subscriber.status();
    println!("Block cache status: {:?}", status);

    Ok(())
}

/// Test block processing with height verification
async fn test_block_processing(config: LocalCacheTestConfig) -> Result<()> {
    let mut env = LocalCacheTestEnvironment::new(&config).await?;

    // Extract finalised state components if available
    let finalised_state = env.block_cache.finalised_state.take();
    let finalised_state_subscriber = env.block_cache_subscriber.finalised_state.take();

    for batch in 1..=config.batch_count {
        println!("Processing batch {}/{}", batch, config.batch_count);

        // Generate blocks
        env.generate_blocks(config.batch_size).await?;
        env.wait_for_sync().await?;

        // Verify heights
        let (validator_height, non_finalised_height, finalised_height) = env.get_heights().await?;

        println!(
            "Heights - Validator: {}, Non-finalised: {}, Finalised: {}",
            validator_height, non_finalised_height, finalised_height
        );

        // Assert height consistency
        assert_eq!(
            validator_height, non_finalised_height,
            "Validator and non-finalised state heights should match"
        );

        if let Some(ref _finalised_state) = finalised_state {
            let expected_finalised = non_finalised_height.saturating_sub(config.batch_size + 1);
            assert_eq!(
                finalised_height,
                expected_finalised,
                "Finalised state should be {} blocks behind non-finalised state",
                config.batch_size + 1
            );
        }

        // Verify block retrieval from non-finalised state
        let mut non_finalised_blocks = Vec::new();
        for height in (finalised_height + 1)..=non_finalised_height {
            let block = env
                .block_cache_subscriber
                .non_finalised_state
                .get_compact_block(HashOrHeight::Height(Height(height)))
                .await
                .with_context(|| format!("Failed to get block at height {height}"))?;
            non_finalised_blocks.push(block);
        }

        // Verify block retrieval from finalised state
        if let Some(ref finalised_subscriber) = finalised_state_subscriber {
            let mut finalised_blocks = Vec::new();
            for height in 1..=finalised_height {
                let block = finalised_subscriber
                    .get_compact_block(HashOrHeight::Height(Height(height)))
                    .await
                    .with_context(|| format!("Failed to get finalised block at height {height}"))?;
                finalised_blocks.push(block);
            }

            println!(
                "Retrieved {} non-finalised blocks and {} finalised blocks",
                non_finalised_blocks.len(),
                finalised_blocks.len()
            );
        }
    }

    Ok(())
}

// Zcashd Tests
mod zcashd {
    use super::*;

    #[tokio::test]
    async fn launch_no_db() {
        test_basic_cache_launch(LocalCacheTestConfig::for_zcashd().without_db())
            .await
            .expect("Zcashd cache launch without DB should succeed");
    }

    #[tokio::test]
    async fn launch_with_db() {
        test_basic_cache_launch(LocalCacheTestConfig::for_zcashd())
            .await
            .expect("Zcashd cache launch with DB should succeed");
    }

    #[tokio::test]
    async fn process_100_blocks() {
        test_block_processing(LocalCacheTestConfig::for_zcashd().with_batches(1))
            .await
            .expect("Zcashd 100-block processing should succeed");
    }

    #[tokio::test]
    async fn process_200_blocks() {
        test_block_processing(LocalCacheTestConfig::for_zcashd().with_batches(2))
            .await
            .expect("Zcashd 200-block processing should succeed");
    }
}

// Zebra Tests
mod zebra {
    use super::*;

    #[tokio::test]
    async fn launch_no_db() {
        test_basic_cache_launch(LocalCacheTestConfig::for_zebra().without_db())
            .await
            .expect("Zebra cache launch without DB should succeed");
    }

    #[tokio::test]
    async fn launch_with_db() {
        test_basic_cache_launch(LocalCacheTestConfig::for_zebra())
            .await
            .expect("Zebra cache launch with DB should succeed");
    }

    #[tokio::test]
    async fn process_100_blocks() {
        test_block_processing(LocalCacheTestConfig::for_zebra().with_batches(1))
            .await
            .expect("Zebra 100-block processing should succeed");
    }

    #[tokio::test]
    async fn process_200_blocks() {
        test_block_processing(LocalCacheTestConfig::for_zebra().with_batches(2))
            .await
            .expect("Zebra 200-block processing should succeed");
    }
}
