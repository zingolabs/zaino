//! Block parsing context types.

use zebra_chain::parameters::Network;

/// Block-level parsing context (separate from transaction context)
#[derive(Debug, Clone)]
pub struct BlockParsingContext {
    /// Network parameters
    pub network: Network,
    /// Expected block height (if known)
    pub height: Option<u32>,
    /// Expected number of transactions (if known from external source)
    pub expected_tx_count: Option<usize>,
    /// Whether to perform strict validation
    pub strict_validation: bool,
}

impl BlockParsingContext {
    /// Create a new block parsing context
    pub fn new(network: Network) -> Self {
        Self {
            network,
            height: None,
            expected_tx_count: None,
            strict_validation: true,
        }
    }

    /// Set the expected block height
    pub fn with_height(mut self, height: u32) -> Self {
        self.height = Some(height);
        self
    }

    /// Set the expected transaction count
    pub fn with_tx_count(mut self, tx_count: usize) -> Self {
        self.expected_tx_count = Some(tx_count);
        self
    }

    /// Disable strict validation (for testing/recovery scenarios)
    pub fn with_relaxed_validation(mut self) -> Self {
        self.strict_validation = false;
        self
    }

    /// Check if the network is mainnet
    pub fn is_mainnet(&self) -> bool {
        matches!(self.network, Network::Mainnet)
    }

    /// Get network parameters
    pub fn network(&self) -> Network {
        self.network
    }

    /// Create a minimal context for testing
    #[cfg(test)]
    pub fn test_context() -> Self {
        Self::new(Network::Mainnet)
            .with_height(100000)
            .with_tx_count(2)
    }
}

/// Transaction parsing context for block-contained transactions
#[derive(Debug, Clone)]  
pub struct BlockTransactionContext<'a> {
    /// Index of this transaction within the block
    pub tx_index: usize,
    /// Transaction ID for this transaction (from external source like RPC)
    pub txid: &'a [u8; 32],
    /// Reference to block parsing context
    pub block_context: &'a BlockParsingContext,
}

impl<'a> BlockTransactionContext<'a> {
    /// Create a new transaction context within a block
    pub fn new(
        tx_index: usize, 
        txid: &'a [u8; 32], 
        block_context: &'a BlockParsingContext
    ) -> Self {
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

    /// Get the expected block height
    pub fn block_height(&self) -> Option<u32> {
        self.block_context.height
    }
}