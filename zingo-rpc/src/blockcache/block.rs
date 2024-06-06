//! Block fetching and deserialization functionality.

/// A block header, containing metadata about a block.
///
/// How are blocks chained together? They are chained together via the
/// backwards reference (previous header hash) present in the block
/// header. Each block points backwards to its parent, all the way
/// back to the genesis block (the first block in the blockchain).
#[derive(Debug)]
pub struct BlockHeaderData {
    /// The block's version field. This is supposed to be `4`:
    ///
    /// > The current and only defined block version number for Zcash is 4.
    ///
    /// but this was not enforced by the consensus rules, and defective mining
    /// software created blocks with other versions, so instead it's effectively
    /// a free field. The only constraint is that it must be at least `4` when
    /// interpreted as an `i32`.
    ///
    /// Size [bytes]: 4
    pub version: i32,

    /// The hash of the previous block, used to create a chain of blocks back to
    /// the genesis block.
    ///
    /// This ensures no previous block can be changed without also changing this
    /// block's header.
    ///
    /// Size [bytes]: 32
    pub hash_prev_block: Vec<u8>,

    /// The root of the Bitcoin-inherited transaction Merkle tree, binding the
    /// block header to the transactions in the block.
    ///
    /// Note that because of a flaw in Bitcoin's design, the `merkle_root` does
    /// not always precisely bind the contents of the block (CVE-2012-2459). It
    /// is sometimes possible for an attacker to create multiple distinct sets of
    /// transactions with the same Merkle root, although only one set will be
    /// valid.
    ///
    /// Size [bytes]: 32
    pub hash_merkle_root: Vec<u8>,

    /// [Pre-Sapling] A reserved field which should be ignored.
    /// [Sapling onward] The root LEBS2OSP_256(rt) of the Sapling note
    /// commitment tree corresponding to the final Sapling treestate of this
    /// block.
    ///
    /// Size [bytes]: 32
    pub hash_final_sapling_root: Vec<u8>,

    /// The block timestamp is a Unix epoch time (UTC) when the miner
    /// started hashing the header (according to the miner).
    ///
    /// Size [bytes]: 4
    pub time: u32,

    /// An encoded version of the target threshold this block's header
    /// hash must be less than or equal to, in the same nBits format
    /// used by Bitcoin.
    ///
    /// For a block at block height `height`, bits MUST be equal to
    /// `ThresholdBits(height)`.
    ///
    /// [Bitcoin-nBits](https://bitcoin.org/en/developer-reference#target-nbits)
    ///
    /// Size [bytes]: 4
    pub n_bits_bytes: Vec<u8>,

    /// An arbitrary field that miners can change to modify the header
    /// hash in order to produce a hash less than or equal to the
    /// target threshold.
    ///
    /// Size [bytes]: 32
    pub nonce: Vec<u8>,

    /// The Equihash solution.
    ///
    /// Size [bytes]: CompactLength
    pub solution: Vec<u8>,
}

/// Complete block header.
#[derive(Debug)]
pub struct FullBlockHeader {
    /// Block header data.
    pub raw_block_header: BlockHeaderData,

    /// Hash of the current block.
    pub cached_hash: Vec<u8>,
}

/// Zingo-Proxy Block.
#[derive(Debug)]
pub struct FullBlock {
    /// The block header, containing block metadata.
    ///
    /// Size [bytes]: 140+CompactLength
    pub hdr: FullBlockHeader,

    /// The block transactions.
    pub vtx: Vec<super::transaction::FullTransaction>,

    /// Block height.
    pub height: i32,
}

// impl parse_from_slice for block_header(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for full_block(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_full_block(&[u8]) -> Result<Self, Error>

// impl to_compact(Self) -> Result<compact_block, Error>

// impl parse_to_compact(&[u8]) -> Result<compact_block, Error>
