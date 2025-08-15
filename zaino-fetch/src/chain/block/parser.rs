//! Block parsing implementation.

use std::io::Cursor;
use crate::{
    error::{ParseError, ValidationError},
    chain::transaction::{FullTransaction, TransactionDispatcher, BlockContext, ActivationHeights},
};
use super::{
    context::{BlockParsingContext, BlockTransactionContext},
    reader::BlockFieldReader,
    fields::{
        BlockVersion, PreviousBlockHash, MerkleRoot, FinalSaplingRoot, 
        BlockTime, NBits, BlockNonce, EquihashSolution, TransactionCount
    },
};
use zebra_chain::{block::Hash, parameters::Network};

/// Raw block header data (before validation)
#[derive(Debug, Clone)]
pub struct RawBlockHeader {
    pub version: i32,
    pub previous_block_hash: Vec<u8>,
    pub merkle_root: Vec<u8>,
    pub final_sapling_root: Vec<u8>,
    pub time: u32,
    pub n_bits: Vec<u8>,
    pub nonce: Vec<u8>,
    pub solution: Vec<u8>,
}

/// Validated block header with convenience methods
#[derive(Debug, Clone)]
pub struct BlockHeader {
    raw: RawBlockHeader,
}

impl BlockHeader {
    /// Create a new block header
    pub fn new(raw: RawBlockHeader) -> Self {
        Self { raw }
    }

    /// Get the block version
    pub fn version(&self) -> i32 {
        self.raw.version
    }

    /// Get the previous block hash
    pub fn previous_block_hash(&self) -> &[u8] {
        &self.raw.previous_block_hash
    }

    /// Get the merkle root
    pub fn merkle_root(&self) -> &[u8] {
        &self.raw.merkle_root
    }

    /// Get the final Sapling root
    pub fn final_sapling_root(&self) -> &[u8] {
        &self.raw.final_sapling_root
    }

    /// Get the block timestamp
    pub fn time(&self) -> u32 {
        self.raw.time
    }

    /// Get the nBits (difficulty target)
    pub fn n_bits(&self) -> &[u8] {
        &self.raw.n_bits
    }

    /// Get the nonce
    pub fn nonce(&self) -> &[u8] {
        &self.raw.nonce
    }

    /// Get the Equihash solution
    pub fn solution(&self) -> &[u8] {
        &self.raw.solution
    }

    /// Get access to raw header data
    pub fn raw(&self) -> &RawBlockHeader {
        &self.raw
    }
}

/// Block header parser
pub struct BlockHeaderParser;

impl BlockHeaderParser {
    /// Parse a block header from raw data
    pub fn parse(
        cursor: &mut Cursor<&[u8]>,
        context: &BlockParsingContext,
    ) -> Result<BlockHeader, ParseError> {
        let raw = Self::read_raw(cursor, context)?;
        let header = Self::validate_and_construct(raw, context)?;
        Ok(header)
    }

    /// Read raw header data with field order validation
    fn read_raw(
        cursor: &mut Cursor<&[u8]>,
        context: &BlockParsingContext,
    ) -> Result<RawBlockHeader, ParseError> {
        let mut reader = BlockFieldReader::new(cursor, context);

        // Block header field order (fixed by Zcash protocol)
        let version = reader.read_field::<BlockVersion>(0)?;
        let previous_block_hash = reader.read_field::<PreviousBlockHash>(1)?;
        let merkle_root = reader.read_field::<MerkleRoot>(2)?;
        let final_sapling_root = reader.read_field::<FinalSaplingRoot>(3)?;
        let time = reader.read_field::<BlockTime>(4)?;
        let n_bits = reader.read_field::<NBits>(5)?;
        let nonce = reader.read_field::<BlockNonce>(6)?;
        let solution = reader.read_field::<EquihashSolution>(7)?;

        Ok(RawBlockHeader {
            version,
            previous_block_hash,
            merkle_root,
            final_sapling_root,
            time,
            n_bits,
            nonce,
            solution,
        })
    }

    /// Validate raw header and construct final header
    fn validate_and_construct(
        raw: RawBlockHeader,
        _context: &BlockParsingContext,
    ) -> Result<BlockHeader, ValidationError> {
        // Additional header-level validation can go here
        // For now, field-level validation is sufficient
        Ok(BlockHeader::new(raw))
    }
}

/// Raw block data (header + transaction data)
#[derive(Debug, Clone)]
pub struct RawBlock {
    pub header: BlockHeader,
    pub transaction_count: u64,
    pub transactions: Vec<FullTransaction>,
}

/// Validated block with convenience methods
#[derive(Debug, Clone)]
pub struct Block {
    raw: RawBlock,
    height: Option<i32>,
}

impl Block {
    /// Create a new block
    pub fn new(raw: RawBlock, height: Option<i32>) -> Self {
        Self { raw, height }
    }

    /// Get the block header
    pub fn header(&self) -> &BlockHeader {
        &self.raw.header
    }

    /// Get the number of transactions
    pub fn transaction_count(&self) -> u64 {
        self.raw.transaction_count
    }

    /// Get the transactions
    pub fn transactions(&self) -> &[FullTransaction] {
        &self.raw.transactions
    }

    /// Get the block height (if known)
    pub fn height(&self) -> Option<i32> {
        self.height
    }

    /// Extract block height from coinbase transaction
    pub fn extract_height_from_coinbase(&self) -> Result<i32, ParseError> {
        if self.raw.transactions.is_empty() {
            return Err(ParseError::InvalidData(
                "Block has no transactions to extract height from".to_string()
            ));
        }

        // Use the same logic as the old block parser
        let transparent_inputs = self.raw.transactions[0].transparent_inputs();
        if transparent_inputs.is_empty() {
            return Err(ParseError::InvalidData(
                "Coinbase transaction has no inputs".to_string()
            ));
        }

        let (_, _, script_sig) = transparent_inputs[0].clone();
        let coinbase_script = script_sig.as_slice();

        let mut cursor = Cursor::new(coinbase_script);
        let height_num: i64 = crate::utils::read_zcash_script_i64(&mut cursor)?;
        
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

    /// Check if this block has any shielded transactions
    pub fn has_shielded_transactions(&self) -> bool {
        self.raw.transactions
            .iter()
            .any(|tx| tx.has_shielded_elements())
    }

    /// Get access to raw block data
    pub fn raw(&self) -> &RawBlock {
        &self.raw
    }
}

/// Main block parser
pub struct BlockParser;

impl BlockParser {
    /// Parse a complete block from raw data with transaction IDs
    /// 
    /// This is the clean API that separates block context from transaction IDs
    pub fn parse_with_txids(
        data: &[u8],
        context: BlockParsingContext,
        txids: Vec<[u8; 32]>,
    ) -> Result<Block, ParseError> {
        let mut cursor = Cursor::new(data);

        // Parse header first (no transaction context needed)
        let header = BlockHeaderParser::parse(&mut cursor, &context)?;

        // Read transaction count
        let mut reader = BlockFieldReader::new(&mut cursor, &context);
        let transaction_count = reader.read_field::<TransactionCount>(0)?;

        // Validate transaction count matches provided txids
        if txids.len() != transaction_count as usize {
            return Err(ParseError::InvalidData(format!(
                "Number of txids ({}) does not match transaction count ({})",
                txids.len(),
                transaction_count
            )));
        }

        // Parse transactions using the new transaction system
        let mut transactions = Vec::with_capacity(transaction_count as usize);
        
        // Create block context for transaction parsing
        let tx_block_context = BlockContext::new(
            context.height.unwrap_or(0),
            Hash([0; 32]), // TODO: Calculate actual block hash
            context.network,
            txids.clone(),
            ActivationHeights::mainnet(), // TODO: Get actual activation heights
            header.time(),
        );

        for (tx_index, txid) in txids.iter().enumerate() {
            // Create transaction context - clean separation of concerns!
            let tx_context = BlockTransactionContext::new(tx_index, txid, &context);
            
            // Parse transaction using FullTransaction (which uses the new system internally)
            let (remaining_data, tx) = FullTransaction::parse_from_slice(
                &data[cursor.position() as usize..],
                Some(vec![txid.to_vec()]), // Still needed for backward compatibility
                None,
            )?;

            transactions.push(tx);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        // Extract block height from coinbase
        let raw_block = RawBlock {
            header,
            transaction_count,
            transactions,
        };
        
        let block = Block::new(raw_block, None);
        let height = block.extract_height_from_coinbase().ok();
        
        Ok(Block::new(block.raw, height))
    }

    /// Parse block header only (for quick header validation)
    pub fn parse_header_only(
        data: &[u8],
        context: BlockParsingContext,
    ) -> Result<BlockHeader, ParseError> {
        let mut cursor = Cursor::new(data);
        BlockHeaderParser::parse(&mut cursor, &context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zebra_chain::parameters::Network;

    fn create_test_context() -> BlockParsingContext {
        BlockParsingContext::new(Network::Mainnet)
            .with_height(100000)
            .with_tx_count(2)
    }

    #[test]
    fn test_block_header_parsing() {
        // Create minimal valid header data
        let mut header_data = Vec::new();
        header_data.extend_from_slice(&4i32.to_le_bytes()); // version
        header_data.extend_from_slice(&[1u8; 32]); // prev hash
        header_data.extend_from_slice(&[2u8; 32]); // merkle root
        header_data.extend_from_slice(&[3u8; 32]); // sapling root
        header_data.extend_from_slice(&1600000000u32.to_le_bytes()); // time
        header_data.extend_from_slice(&[4u8; 4]); // n_bits
        header_data.extend_from_slice(&[5u8; 32]); // nonce
        
        // Add solution with CompactSize prefix
        header_data.push(0xF0); // CompactSize for 1344 bytes
        header_data.push(0x05);
        header_data.extend_from_slice(&[6u8; 1344]); // solution

        let context = create_test_context();
        let result = BlockParser::parse_header_only(&header_data, context);
        
        assert!(result.is_ok());
        let header = result.unwrap();
        assert_eq!(header.version(), 4);
        assert_eq!(header.time(), 1600000000);
    }

    #[test]
    fn test_block_header_field_order_validation() {
        // This would require implementing a test that scrambles field order
        // to verify the field reader catches order violations
    }

    #[test]
    fn test_block_transaction_context() {
        let parsing_context = create_test_context();
        let txid = [1u8; 32];
        
        let tx_context = BlockTransactionContext::new(0, &txid, &parsing_context);
        
        assert!(tx_context.is_coinbase());
        assert_eq!(tx_context.network(), Network::Mainnet);
        assert_eq!(tx_context.block_height(), Some(100000));
    }
}