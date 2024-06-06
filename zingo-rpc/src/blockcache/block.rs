//! Block fetching and deserialization functionality.

/// Block header data.
#[derive(Debug)]
pub struct BlockHeaderData {
    pub version: i32,
    pub hash_prev_block: Vec<u8>,
    pub hash_merkle_root: Vec<u8>,
    pub hash_final_sapling_root: Vec<u8>,
    pub time: u32,
    pub n_bits_bytes: Vec<u8>,
    pub nonce: Vec<u8>,
    pub solution: Vec<u8>,
}

/// Complete block header.
#[derive(Debug)]
pub struct FullBlockHeader {
    pub raw_block_header: BlockHeaderData,
    pub cached_hash: Vec<u8>,
}

/// Zingo-Proxy Block.
#[derive(Debug)]
pub struct FullBlock {
    pub hdr: FullBlockHeader,
    pub vtx: Vec<super::transaction::FullTransaction>,
    pub height: i32,
}

// impl parse_from_slice for block_header(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for full_block(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_full_block(&[u8]) -> Result<Self, Error>

// impl to_compact(Self) -> Result<compact_block, Error>

// impl parse_to_compact(&[u8]) -> Result<compact_block, Error>
