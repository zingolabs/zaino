//! Block fetching and deserialization functionality.

use crate::{
    chain::{
        error::{BlockCacheError, ParseError},
        transaction::FullTransaction,
        utils::{
            display_txids_to_server, read_bytes, read_i32, read_u32, read_zcash_script_i64,
            CompactSize, ParseFromSlice,
        },
    },
    jsonrpc::{connector::JsonRpcConnector, response::GetBlockResponse},
};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use zaino_proto::proto::compact_formats::{ChainMetadata, CompactBlock};

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

impl ParseFromSlice for BlockHeaderData {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
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
        let mut cursor = Cursor::new(data);

        let version = read_i32(&mut cursor, "Error reading BlockHeaderData::version")?;
        let hash_prev_block = read_bytes(
            &mut cursor,
            32,
            "Error reading BlockHeaderData::hash_prev_block",
        )?;
        let hash_merkle_root = read_bytes(
            &mut cursor,
            32,
            "Error reading BlockHeaderData::hash_merkle_root",
        )?;
        let hash_final_sapling_root = read_bytes(
            &mut cursor,
            32,
            "Error reading BlockHeaderData::hash_final_sapling_root",
        )?;
        let time = read_u32(&mut cursor, "Error reading BlockHeaderData::time")?;
        let n_bits_bytes = read_bytes(
            &mut cursor,
            4,
            "Error reading BlockHeaderData::n_bits_bytes",
        )?;
        let nonce = read_bytes(&mut cursor, 32, "Error reading BlockHeaderData::nonce")?;

        let solution = {
            let compact_length = CompactSize::read(&mut cursor)?;
            read_bytes(
                &mut cursor,
                compact_length as usize,
                "Error reading BlockHeaderData::solution",
            )?
        };

        Ok((
            &data[cursor.position() as usize..],
            BlockHeaderData {
                version,
                hash_prev_block,
                hash_merkle_root,
                hash_final_sapling_root,
                time,
                n_bits_bytes,
                nonce,
                solution,
            },
        ))
    }
}

impl BlockHeaderData {
    /// Serializes the block header into a byte vector.
    pub fn to_binary(&self) -> Result<Vec<u8>, ParseError> {
        let mut buffer = Vec::new();

        buffer.extend(&self.version.to_le_bytes());
        buffer.extend(&self.hash_prev_block);
        buffer.extend(&self.hash_merkle_root);
        buffer.extend(&self.hash_final_sapling_root);
        buffer.extend(&self.time.to_le_bytes());
        buffer.extend(&self.n_bits_bytes);
        buffer.extend(&self.nonce);
        let mut solution_compact_size = Vec::new();
        CompactSize::write(&mut solution_compact_size, self.solution.len())?;
        buffer.extend(solution_compact_size);
        buffer.extend(&self.solution);

        Ok(buffer)
    }

    /// Extracts the block hash from the block header.
    pub fn get_hash(&self) -> Result<Vec<u8>, ParseError> {
        let serialized_header = self.to_binary()?;

        let mut hasher = Sha256::new();
        hasher.update(&serialized_header);
        let digest = hasher.finalize_reset();
        hasher.update(digest);
        let final_digest = hasher.finalize();

        Ok(final_digest.to_vec())
    }
}

/// Complete block header.
#[derive(Debug)]
pub struct FullBlockHeader {
    /// Block header data.
    pub raw_block_header: BlockHeaderData,

    /// Hash of the current block.
    pub cached_hash: Vec<u8>,
}

/// Zingo-Indexer Block.
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

impl ParseFromSlice for FullBlock {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        let txid = txid.ok_or_else(|| {
            ParseError::InvalidData("txid must be used for FullBlock::parse_from_slice".to_string())
        })?;
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for FullBlock::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let (remaining_data, block_header_data) =
            BlockHeaderData::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        let tx_count = CompactSize::read(&mut cursor)?;
        if txid.len() != tx_count as usize {
            return Err(ParseError::InvalidData(format!(
                "number of txids ({}) does not match tx_count ({})",
                txid.len(),
                tx_count
            )));
        }
        let mut transactions = Vec::with_capacity(tx_count as usize);
        let mut remaining_data = &data[cursor.position() as usize..];
        for txid_item in txid.iter() {
            if remaining_data.is_empty() {
                return Err(ParseError::InvalidData(
                    "parsing block transactions: not enough data for transaction.".to_string(),
                ));
            }
            let (new_remaining_data, tx) = FullTransaction::parse_from_slice(
                &data[cursor.position() as usize..],
                Some(vec![txid_item.clone()]),
                None,
            )?;
            transactions.push(tx);
            remaining_data = new_remaining_data;
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }
        let block_height = Self::get_block_height(&transactions)?;
        let block_hash = block_header_data.get_hash()?;

        Ok((
            remaining_data,
            FullBlock {
                hdr: FullBlockHeader {
                    raw_block_header: block_header_data,
                    cached_hash: block_hash,
                },
                vtx: transactions,
                height: block_height,
            },
        ))
    }
}

/// Genesis block special case.
///
/// From LightWalletD:
/// see https://github.com/zcash/lightwalletd/issues/17#issuecomment-467110828.
const GENESIS_TARGET_DIFFICULTY: u32 = 520617983;

impl FullBlock {
    /// Extracts the block height from the coinbase transaction.
    pub fn get_block_height(transactions: &[FullTransaction]) -> Result<i32, ParseError> {
        let coinbase_script = transactions[0].raw_transaction.transparent_inputs[0]
            .script_sig
            .as_slice();
        let mut cursor = Cursor::new(coinbase_script);

        let height_num: i64 = read_zcash_script_i64(&mut cursor)?;
        if height_num < 0 {
            return Ok(-1);
        }
        if height_num > i64::from(u32::MAX) {
            return Ok(-1);
        }
        if (height_num as u32) == GENESIS_TARGET_DIFFICULTY {
            return Ok(0);
        }

        Ok(height_num as i32)
    }

    /// Decodes a hex encoded zcash full block into a FullBlock struct.
    pub fn parse_full_block(data: &[u8], txid: Option<Vec<Vec<u8>>>) -> Result<Self, ParseError> {
        let (remaining_data, full_block) = Self::parse_from_slice(data, txid, None)?;
        if !remaining_data.is_empty() {
            return Err(ParseError::InvalidData(format!(
                "Error decoding full block - {} bytes of Remaining data. Compact Block Created: ({:?})",
                remaining_data.len(),
                full_block.to_compact(0, 0)
            )));
        }
        Ok(full_block)
    }

    /// Converts a zcash full block into a compact block.
    pub fn to_compact(
        self,
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
    ) -> Result<CompactBlock, ParseError> {
        let vtx = self
            .vtx
            .into_iter()
            .enumerate()
            .filter_map(|(index, tx)| {
                if tx.has_shielded_elements() {
                    Some(tx.to_compact(index as u64))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        // NOTE: LightWalletD doesnt return a compact block header, however this could be used to return data if useful.
        // let header = self.hdr.raw_block_header.to_binary()?;
        let header = Vec::new();

        let compact_block = CompactBlock {
            proto_version: 1, // TODO: check this is correct!
            height: self.height as u64,
            hash: self.hdr.cached_hash.clone(),
            prev_hash: self.hdr.raw_block_header.hash_prev_block.clone(),
            time: self.hdr.raw_block_header.time,
            header,
            vtx,
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size,
                orchard_commitment_tree_size,
            }),
        };

        Ok(compact_block)
    }

    /// Decodes a hex encoded zcash full block into a CompactBlock struct.
    pub fn parse_to_compact(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
    ) -> Result<CompactBlock, ParseError> {
        Self::parse_full_block(data, txid)?
            .to_compact(sapling_commitment_tree_size, orchard_commitment_tree_size)
    }
}

/// Returns a compact block.
///
/// Retrieves a full block from zebrad/zcashd using 2 get_block calls.
/// This is because a get_block verbose = 1 call is require to fetch txids.
/// TODO: Save retrieved CompactBlock to the BlockCache.
/// TODO: Return more representative error type.
pub async fn get_block_from_node(
    zebra_uri: &http::Uri,
    height: &u32,
) -> Result<CompactBlock, BlockCacheError> {
    let zebrad_client = JsonRpcConnector::new(
        zebra_uri.clone(),
        Some("xxxxxx".to_string()),
        Some("xxxxxx".to_string()),
    )
    .await?;
    let block_1 = zebrad_client.get_block(height.to_string(), Some(1)).await;
    match block_1 {
        Ok(GetBlockResponse::Object {
            hash,
            confirmations: _,
            height: _,
            time: _,
            tx,
            trees,
        }) => {
            let block_0 = zebrad_client.get_block(hash.0.to_string(), Some(0)).await;
            match block_0 {
                Ok(GetBlockResponse::Object {
                    hash: _,
                    confirmations: _,
                    height: _,
                    time: _,
                    tx: _,
                    trees: _,
                }) => Err(BlockCacheError::ParseError(ParseError::InvalidData(
                    "Received object block type, this should not be possible here.".to_string(),
                ))),
                Ok(GetBlockResponse::Raw(block_hex)) => Ok(FullBlock::parse_to_compact(
                    block_hex.as_ref(),
                    Some(display_txids_to_server(tx)?),
                    trees.sapling() as u32,
                    trees.orchard() as u32,
                )?),
                Err(e) => Err(e.into()),
            }
        }
        Ok(GetBlockResponse::Raw(_)) => Err(BlockCacheError::ParseError(ParseError::InvalidData(
            "Received raw block type, this should not be possible here.".to_string(),
        ))),
        Err(e) => Err(e.into()),
    }
}
