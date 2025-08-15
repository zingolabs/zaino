//! Block parsing and validation using modern cursor-based architecture.
//!
//! This module provides clean, cursor-based block parsing with proper
//! separation between block context and transaction context.

use std::io::Cursor;
use crate::{
    error::ParseError,
    utils::ParseFromHex,
    chain::{
        types::{TxId, TxIdBytes, CanonicalTxIdList, txid_list_to_canonical},
        transaction::{FullTransaction, FullTransactionContext, BlockContext, ActivationHeights},
    },
};
use zaino_proto::proto::compact_formats::{ChainMetadata, CompactBlock};
use zebra_chain::{block::Hash, parameters::Network};

/// Block header data
#[derive(Debug, Clone)]
pub struct BlockHeaderData {
    /// The block's version field
    pub version: i32,
    /// The hash of the previous block
    pub hash_prev_block: Vec<u8>,
    /// The root of the transaction Merkle tree
    pub hash_merkle_root: Vec<u8>,
    /// The root of the Sapling note commitment tree
    pub hash_final_sapling_root: Vec<u8>,
    /// The block timestamp
    pub time: u32,
    /// The difficulty target
    pub n_bits_bytes: Vec<u8>,
    /// The nonce
    pub nonce: Vec<u8>,
    /// The Equihash solution
    pub solution: Vec<u8>,
}

impl BlockHeaderData {
    /// Get the block version
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Get the previous block hash
    pub fn hash_prev_block(&self) -> &[u8] {
        &self.hash_prev_block
    }

    /// Get the merkle root
    pub fn hash_merkle_root(&self) -> &[u8] {
        &self.hash_merkle_root
    }

    /// Get the final Sapling root
    pub fn hash_final_sapling_root(&self) -> &[u8] {
        &self.hash_final_sapling_root
    }

    /// Get the block timestamp
    pub fn time(&self) -> u32 {
        self.time
    }

    /// Get the nBits
    pub fn n_bits_bytes(&self) -> &[u8] {
        &self.n_bits_bytes
    }

    /// Get the nonce
    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }

    /// Get the Equihash solution
    pub fn solution(&self) -> &[u8] {
        &self.solution
    }
}

/// Context for parsing block headers
#[derive(Debug, Clone)]
pub struct BlockHeaderContext {
    /// Network parameters
    pub network: Network,
    /// Whether to perform strict validation
    pub strict_validation: bool,
}

impl BlockHeaderContext {
    /// Create a new context
    pub fn new(network: Network) -> Self {
        Self {
            network,
            strict_validation: true,
        }
    }

    /// Create context with relaxed validation
    pub fn with_relaxed_validation(mut self) -> Self {
        self.strict_validation = false;
        self
    }
}

impl ParseFromHex for BlockHeaderData {
    type Context = BlockHeaderContext;

    fn parse_from_cursor(
        cursor: &mut Cursor<&[u8]>,
        context: Self::Context,
    ) -> Result<Self, ParseError> {
        use crate::utils::{read_bytes, read_i32, read_u32, CompactSize};

        let version = read_i32(cursor, "Error reading BlockHeaderData::version")?;
        
        // Validate version
        if context.strict_validation && version < 4 {
            return Err(ParseError::InvalidData(format!(
                "Block version {} must be at least 4", version
            )));
        }

        let hash_prev_block = read_bytes(cursor, 32, "Error reading BlockHeaderData::hash_prev_block")?;
        let hash_merkle_root = read_bytes(cursor, 32, "Error reading BlockHeaderData::hash_merkle_root")?;
        let hash_final_sapling_root = read_bytes(cursor, 32, "Error reading BlockHeaderData::hash_final_sapling_root")?;
        let time = read_u32(cursor, "Error reading BlockHeaderData::time")?;
        let n_bits_bytes = read_bytes(cursor, 4, "Error reading BlockHeaderData::n_bits_bytes")?;
        let nonce = read_bytes(cursor, 32, "Error reading BlockHeaderData::nonce")?;

        // Read solution with CompactSize prefix
        let solution_length = CompactSize::read(cursor)?;
        let solution = read_bytes(cursor, solution_length as usize, "Error reading BlockHeaderData::solution")?;

        // Validate solution length for mainnet
        if context.strict_validation && 
           matches!(context.network, Network::Mainnet) && 
           solution.len() != 1344 {
            return Err(ParseError::InvalidData(format!(
                "Equihash solution length {} does not match expected 1344 for mainnet", 
                solution.len()
            )));
        }

        Ok(BlockHeaderData {
            version,
            hash_prev_block,
            hash_merkle_root,
            hash_final_sapling_root,
            time,
            n_bits_bytes,
            nonce,
            solution,
        })
    }
}

/// Full block data
#[derive(Debug, Clone)]
pub struct FullBlock {
    /// Block header
    pub header: BlockHeaderData,
    /// List of transactions
    pub vtx: Vec<FullTransaction>,
    /// Block height
    pub height: i32,
}

impl FullBlock {
    /// Create a new full block
    pub fn new(header: BlockHeaderData, vtx: Vec<FullTransaction>, height: i32) -> Self {
        Self { header, vtx, height }
    }

    /// Get the block header
    pub fn header(&self) -> &BlockHeaderData {
        &self.header
    }

    /// Get the transactions
    pub fn transactions(&self) -> &[FullTransaction] {
        &self.vtx
    }

    /// Get the block height
    pub fn height(&self) -> i32 {
        self.height
    }

    /// Get transaction count
    pub fn tx_count(&self) -> u64 {
        self.vtx.len() as u64
    }

    /// Check if block has shielded transactions
    pub fn has_shielded_transactions(&self) -> bool {
        self.vtx.iter().any(|tx| tx.has_shielded_elements())
    }

    /// Get block time from header
    pub fn time(&self) -> u32 {
        self.header.time
    }

    /// Extract block height from coinbase transaction
    pub fn get_block_height(transactions: &[FullTransaction]) -> Result<i32, ParseError> {
        use crate::utils::read_zcash_script_i64;

        if transactions.is_empty() {
            return Err(ParseError::InvalidData(
                "Cannot extract height from empty transaction list".to_string()
            ));
        }

        let transparent_inputs = transactions[0].transparent_inputs();
        if transparent_inputs.is_empty() {
            return Err(ParseError::InvalidData(
                "Coinbase transaction has no inputs".to_string()
            ));
        }

        let (_, _, script_sig) = transparent_inputs[0].clone();
        let coinbase_script = script_sig.as_slice();

        let mut cursor = Cursor::new(coinbase_script);
        let height_num: i64 = read_zcash_script_i64(&mut cursor)?;
        
        if height_num < 0 {
            return Ok(-1);
        }
        if height_num > i64::from(u32::MAX) {
            return Ok(-1);
        }
        
        // Check for genesis block special case
        const GENESIS_TARGET_DIFFICULTY: u32 = 520617983;
        if (height_num as u32) == GENESIS_TARGET_DIFFICULTY {
            return Ok(0);
        }

        Ok(height_num as i32)
    }
}

/// Context for parsing full blocks
#[derive(Debug, Clone)]
pub struct FullBlockContext {
    /// Network parameters
    pub network: Network,
    /// Transaction IDs from external source (RPC)
    pub txids: CanonicalTxIdList,
    /// Whether to perform strict validation
    pub strict_validation: bool,
    /// Expected block height (if known)
    pub height: Option<u32>,
}

impl FullBlockContext {
    /// Create a new context with transaction IDs
    pub fn new(network: Network, txids: CanonicalTxIdList) -> Self {
        Self {
            network,
            txids,
            strict_validation: true,
            height: None,
        }
    }

    /// Create context from legacy txid format
    pub fn from_legacy_txids(network: Network, legacy_txids: Vec<Vec<u8>>) -> Result<Self, ParseError> {
        let txids = txid_list_to_canonical(legacy_txids)?;
        Ok(Self::new(network, txids))
    }

    /// Set expected block height
    pub fn with_height(mut self, height: u32) -> Self {
        self.height = Some(height);
        self
    }

    /// Create context with relaxed validation
    pub fn with_relaxed_validation(mut self) -> Self {
        self.strict_validation = false;
        self
    }
}

impl ParseFromHex for FullBlock {
    type Context = FullBlockContext;

    fn parse_from_cursor(
        cursor: &mut Cursor<&[u8]>,
        context: Self::Context,
    ) -> Result<Self, ParseError> {
        use crate::utils::CompactSize;

        // Parse header
        let header_context = BlockHeaderContext::new(context.network)
            .with_relaxed_validation(); // Use relaxed validation for compatibility
        let header = BlockHeaderData::parse_from_cursor(cursor, header_context)?;

        // Read transaction count
        let tx_count = CompactSize::read(cursor)?;
        
        // Validate transaction count matches provided txids
        if context.txids.len() != tx_count as usize {
            return Err(ParseError::InvalidData(format!(
                "Number of txids ({}) does not match transaction count ({})",
                context.txids.len(),
                tx_count
            )));
        }

        // Create block context for transaction parsing
        let block_context = BlockContext::new(
            context.height.unwrap_or(0),
            Hash([0; 32]), // TODO: Calculate actual block hash
            context.network,
            context.txids.clone(),
            ActivationHeights::mainnet(), // TODO: Get actual activation heights for network
            header.time,
        );

        // Parse transactions
        let mut transactions = Vec::with_capacity(tx_count as usize);
        for (tx_index, txid) in context.txids.iter().enumerate() {
            let tx_context = FullTransactionContext::with_block_context(
                txid.to_vec(),
                block_context.clone(),
            );
            
            let transaction = FullTransaction::parse_from_cursor(cursor, tx_context)?;
            transactions.push(transaction);
        }

        // Extract block height from coinbase
        let height = Self::get_block_height(&transactions).unwrap_or(-1);

        Ok(FullBlock::new(header, transactions, height))
    }
}

/// Temporary compatibility methods for old API
impl FullBlock {
    /// Parse from slice using old API - DEPRECATED
    pub fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        _tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        let legacy_txids = txid.ok_or_else(|| {
            ParseError::InvalidData("txid must be used for FullBlock::parse_from_slice".to_string())
        })?;

        let context = FullBlockContext::from_legacy_txids(Network::Mainnet, legacy_txids)?;
        let (remaining, block) = Self::parse_from_slice(data, context)?;
        
        Ok((remaining, block))
    }

    /// Parse from hex string with transaction IDs
    pub fn parse_from_hex(
        hex_data: &str,
        txids: Option<Vec<Vec<u8>>>,
    ) -> Result<Self, ParseError> {
        let data = hex::decode(hex_data)
            .map_err(|e| ParseError::InvalidData(format!("Invalid hex: {}", e)))?;
        
        let legacy_txids = txids.ok_or_else(|| {
            ParseError::InvalidData("Transaction IDs required".to_string())
        })?;

        let context = FullBlockContext::from_legacy_txids(Network::Mainnet, legacy_txids)?;
        let (_, block) = Self::parse_from_slice(&data, context)?;
        
        Ok(block)
    }
}

/// Temporary compatibility for BlockHeaderData old API
impl BlockHeaderData {
    /// Parse from slice using old API - DEPRECATED  
    pub fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        // Validate that header parsing doesn't need transaction parameters
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for BlockHeaderData::parse_from_slice".to_string(),
            ));
        }
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for BlockHeaderData::parse_from_slice".to_string(),
            ));
        }

        let context = BlockHeaderContext::new(Network::Mainnet)
            .with_relaxed_validation();
        let (remaining, header) = Self::parse_from_slice(data, context)?;
        
        Ok((remaining, header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_header_context() {
        let context = BlockHeaderContext::new(Network::Mainnet);
        assert!(context.strict_validation);
        assert!(matches!(context.network, Network::Mainnet));
    }

    #[test]
    fn test_full_block_context() {
        let txids = vec![[1u8; 32], [2u8; 32]];
        let context = FullBlockContext::new(Network::Mainnet, txids.clone());
        
        assert_eq!(context.txids.len(), 2);
        assert!(context.strict_validation);
        assert!(matches!(context.network, Network::Mainnet));
    }

    #[test]
    fn test_legacy_txid_conversion() {
        let legacy_txids = vec![vec![1u8; 32], vec![2u8; 32]];
        let context = FullBlockContext::from_legacy_txids(Network::Mainnet, legacy_txids);
        
        assert!(context.is_ok());
        let context = context.unwrap();
        assert_eq!(context.txids.len(), 2);
    }
}