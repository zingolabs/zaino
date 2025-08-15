//! Transaction parsing context types.

use zebra_chain::{block::Hash, parameters::Network};

/// Transaction ID type alias for clarity
pub type TxId = [u8; 32];

/// Block-level context for transaction parsing
#[derive(Debug, Clone)]
pub struct BlockContext {
    /// Block height
    pub height: u32,
    /// Block hash
    pub hash: Hash,
    /// Network parameters
    pub network: Network,
    /// All transaction IDs in this block (in order)
    pub txids: Vec<TxId>,
    /// Network activation heights
    pub activation_heights: ActivationHeights,
    /// Block timestamp
    pub timestamp: u32,
}

impl BlockContext {
    /// Create a new block context
    pub fn new(
        height: u32,
        hash: Hash,
        network: Network,
        txids: Vec<TxId>,
        activation_heights: ActivationHeights,
        timestamp: u32,
    ) -> Self {
        Self {
            height,
            hash,
            network,
            txids,
            activation_heights,
            timestamp,
        }
    }

    /// Check if Overwinter is active at this block height
    pub fn is_overwinter_active(&self) -> bool {
        self.height >= self.activation_heights.overwinter
    }

    /// Check if Sapling is active at this block height
    pub fn is_sapling_active(&self) -> bool {
        self.height >= self.activation_heights.sapling
    }

    /// Check if Heartwood is active at this block height
    pub fn is_heartwood_active(&self) -> bool {
        self.height >= self.activation_heights.heartwood
    }

    /// Create a minimal context for testing
    #[cfg(test)]
    pub fn test_context() -> Self {
        Self::new(
            100000, // Some reasonable test height
            Hash([0; 32]),
            Network::Mainnet,
            vec![[1; 32], [2; 32]], // Test txids
            ActivationHeights::mainnet(),
            1234567890, // Test timestamp
        )
    }

    /// Create a minimal context for parsing when block context is unknown
    pub fn minimal_for_parsing() -> Self {
        Self::new(
            1000000, // High enough for all upgrades to be active
            Hash([0; 32]),
            Network::Mainnet,
            Vec::new(), // Will be populated during parsing
            ActivationHeights::mainnet(),
            0,
        )
    }
}

/// Transaction-level context for parsing
#[derive(Debug, Clone)]
pub struct TransactionContext<'a> {
    /// Index of this transaction within the block
    pub tx_index: usize,
    /// Transaction ID for this transaction
    pub txid: &'a TxId,
    /// Reference to block context
    pub block_context: &'a BlockContext,
}

impl<'a> TransactionContext<'a> {
    /// Create a new transaction context
    pub fn new(tx_index: usize, txid: &'a TxId, block_context: &'a BlockContext) -> Self {
        Self {
            tx_index,
            txid,
            block_context,
        }
    }

    /// Check if this is the first transaction in the block (coinbase)
    pub fn is_coinbase(&self) -> bool {
        self.tx_index == 0
    }

    /// Get the network for this transaction
    pub fn network(&self) -> Network {
        self.block_context.network
    }

    /// Get the block height for this transaction
    pub fn block_height(&self) -> u32 {
        self.block_context.height
    }
}

/// Network activation heights for different upgrades
#[derive(Debug, Clone)]
pub struct ActivationHeights {
    pub overwinter: u32,
    pub sapling: u32,
    pub blossom: u32,
    pub heartwood: u32,
    pub canopy: u32,
    pub nu5: u32,
}

impl ActivationHeights {
    /// Mainnet activation heights
    pub fn mainnet() -> Self {
        Self {
            overwinter: 347500,
            sapling: 419200,
            blossom: 653600,
            heartwood: 903000,
            canopy: 1046400,
            nu5: 1687104,
        }
    }

    /// Testnet activation heights
    pub fn testnet() -> Self {
        Self {
            overwinter: 207500,
            sapling: 280000,
            blossom: 584000,
            heartwood: 903800,
            canopy: 1028500,
            nu5: 1842420,
        }
    }

    /// Regtest activation heights (everything active immediately)
    pub fn regtest() -> Self {
        Self {
            overwinter: 0,
            sapling: 0,
            blossom: 0,
            heartwood: 0,
            canopy: 0,
            nu5: 0,
        }
    }
}