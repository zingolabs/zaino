//! What ZainoDB needs to be configured that a chain store in general does not.
//!
//! A store is configured from two pieces: [`ChainStoreConfig`], which every
//! implementation takes, and [`ZainoDbConfig`], which is this one's. The split
//! is not ceremony — it is the two things that genuinely cannot live in a
//! domain crate:
//!
//! - the LMDB sizing and write-cadence budgets, which describe *this* storage
//!   engine and mean nothing to another, and
//! - the network, which the store needs only to know which pools have activated
//!   at a height, and which is a `zebra-chain` type the domain crate must not
//!   name.
//!
//! Everything else — where the store lives and how it behaves when a build
//! fails — is the same question for any store, and lives in
//! [`ChainStoreConfig`].

use zaino_chain_store::ChainStoreConfig;
use zaino_common::{AccumulatorRebuildMemorySize, DatabaseSize, StorageConfig, SyncWriteBatchSize};

/// The ZainoDB-specific half of a store's configuration.
///
/// Fields are private for the reason [`ChainStoreConfig`]'s are: the budgets
/// here are read on hot paths that cannot re-validate them, and a caller that
/// can only reach them through accessors cannot leave one in a state the store
/// then has to defend against.
///
/// Deliberately carries **no path**. Where the store lives is
/// [`ChainStoreConfig::path`], and a second copy here would be a second answer
/// to one question.
#[derive(Debug, Clone)]
pub struct ZainoDbConfig {
    size: DatabaseSize,
    sync_write_batch_size: SyncWriteBatchSize,
    accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize,
    sync_checkpoint_interval: u64,
    network: zebra_chain::parameters::Network,
}

impl ZainoDbConfig {
    /// The storage defaults, against `network`.
    ///
    /// A network has no default worth guessing — a store built against the
    /// wrong activation schedule writes wrong commitment-tree rows — so it is
    /// the one thing this cannot fill in, and therefore the only argument.
    pub fn new(network: zebra_chain::parameters::Network) -> Self {
        Self::from_storage(&StorageConfig::default(), network)
    }

    /// The budgets an operator configured, against `network`.
    ///
    /// Takes the whole [`StorageConfig`] and reads the four budgets out of it,
    /// ignoring its `path`: the path an operator configures reaches the store
    /// through [`ChainStoreConfig::at_path`]. The cache settings are not read
    /// here either — nothing in this crate consults them.
    pub fn from_storage(
        storage: &StorageConfig,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        Self {
            size: storage.database.size,
            sync_write_batch_size: storage.database.sync_write_batch_size,
            accumulator_rebuild_memory_size: storage.database.accumulator_rebuild_memory_size,
            sync_checkpoint_interval: storage.database.sync_checkpoint_interval,
            network,
        }
    }

    /// Maximum size of the LMDB environment.
    pub fn size(&self) -> DatabaseSize {
        self.size
    }

    /// Heap budget for a bulk-sync write batch.
    pub fn sync_write_batch_size(&self) -> SyncWriteBatchSize {
        self.sync_write_batch_size
    }

    /// Heap budget for the txout-set accumulator rebuild's spent set.
    pub fn accumulator_rebuild_memory_size(&self) -> AccumulatorRebuildMemorySize {
        self.accumulator_rebuild_memory_size
    }

    /// Seconds between durability checkpoints during a bulk sync.
    pub fn sync_checkpoint_interval(&self) -> u64 {
        self.sync_checkpoint_interval
    }

    /// The activation schedule the store builds against.
    pub fn network(&self) -> &zebra_chain::parameters::Network {
        &self.network
    }

    /// Set the LMDB environment size.
    pub fn set_size(&mut self, size: DatabaseSize) {
        self.size = size;
    }

    /// Set the bulk-sync write-batch budget.
    pub fn set_sync_write_batch_size(&mut self, size: SyncWriteBatchSize) {
        self.sync_write_batch_size = size;
    }

    /// Set the accumulator-rebuild memory budget.
    pub fn set_accumulator_rebuild_memory_size(&mut self, size: AccumulatorRebuildMemorySize) {
        self.accumulator_rebuild_memory_size = size;
    }

    /// Set the durability-checkpoint interval, in seconds.
    pub fn set_sync_checkpoint_interval(&mut self, seconds: u64) {
        self.sync_checkpoint_interval = seconds;
    }
}

/// Both halves of a running store's configuration, as this crate threads them.
///
/// Internal: the two are separate at the boundary because they belong to
/// different crates, and paired here because everything below `spawn` needs
/// both and passing two parameters through every layer would be noise.
#[derive(Debug, Clone)]
pub(crate) struct StoreSettings {
    pub(crate) store: ChainStoreConfig,
    pub(crate) db: ZainoDbConfig,
}

impl StoreSettings {
    pub(crate) fn new(store: ChainStoreConfig, db: ZainoDbConfig) -> Self {
        Self { store, db }
    }
}
