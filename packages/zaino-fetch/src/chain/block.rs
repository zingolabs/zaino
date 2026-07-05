//! Block fetching and deserialization functionality.

use crate::{
    chain::{error::ParseError, transaction::FullTransaction},
    utils::ParseFromSlice,
};
use std::{io::Cursor, sync::Arc};
use zaino_proto::proto::{
    compact_formats::{ChainMetadata, CompactBlock},
    utils::PoolTypeFilter,
};
use zebra_chain::serialization::{ZcashDeserialize as _, ZcashSerialize as _};

/// Complete block header.
#[derive(Debug, Clone)]
pub struct FullBlockHeader {
    header: Arc<zebra_chain::block::Header>,
    raw_bytes: Vec<u8>,
    cached_hash: Vec<u8>,
}

impl FullBlockHeader {
    fn from_zebra(header: Arc<zebra_chain::block::Header>) -> Result<Self, ParseError> {
        let raw_bytes = header.zcash_serialize_to_vec()?;
        let cached_hash = header.hash().0.to_vec();

        Ok(Self {
            header,
            raw_bytes,
            cached_hash,
        })
    }

    /// Returns the Zcash block version.
    pub fn version(&self) -> i32 {
        i32::from_le_bytes(self.header.version.to_le_bytes())
    }

    /// Returns The hash of the previous block.
    pub fn hash_prev_block(&self) -> Vec<u8> {
        self.header.previous_block_hash.0.to_vec()
    }

    /// Returns the root of the Bitcoin-inherited transaction Merkle tree.
    pub fn hash_merkle_root(&self) -> Vec<u8> {
        self.header.merkle_root.0.to_vec()
    }

    /// Returns the final sapling root of the block.
    pub fn final_sapling_root(&self) -> Vec<u8> {
        self.header.commitment_bytes.to_vec()
    }

    /// Returns the time when the miner started hashing the header (according to the miner).
    pub fn time(&self) -> u32 {
        u32::try_from(self.header.time.timestamp())
            .expect("deserialized block header timestamps fit in u32")
    }

    /// Returns an encoded version of the target threshold.
    pub fn n_bits_bytes(&self) -> Vec<u8> {
        self.raw_bytes[104..108].to_vec()
    }

    /// Returns the block's nonce.
    pub fn nonce(&self) -> Vec<u8> {
        self.header.nonce.to_vec()
    }

    /// Returns the block's Equihash solution.
    pub fn solution(&self) -> Vec<u8> {
        match &self.header.solution {
            zebra_chain::work::equihash::Solution::Common(solution) => solution.to_vec(),
            zebra_chain::work::equihash::Solution::Regtest(solution) => solution.to_vec(),
        }
    }

    /// Returns the Hash of the current block.
    pub fn cached_hash(&self) -> Vec<u8> {
        self.cached_hash.clone()
    }
}

/// Zingo-Indexer Block.
#[derive(Debug, Clone)]
pub struct FullBlock {
    /// The block header, containing block metadata.
    hdr: FullBlockHeader,

    /// The block transactions.
    vtx: Vec<super::transaction::FullTransaction>,

    /// Block height.
    height: i32,
}

impl ParseFromSlice for FullBlock {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for FullBlock::parse_from_slice".to_string(),
            ));
        }

        let mut cursor = Cursor::new(data);
        let block = zebra_chain::block::Block::zcash_deserialize(&mut cursor)?;
        let consumed = usize::try_from(cursor.position())?;
        let txids = txid.unwrap_or_default();

        if !txids.is_empty() && txids.len() != block.transactions.len() {
            return Err(ParseError::InvalidData(format!(
                "number of txids ({}) does not match tx_count ({})",
                txids.len(),
                block.transactions.len()
            )));
        }

        let hdr = FullBlockHeader::from_zebra(block.header.clone())?;
        let vtx = block
            .transactions
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, transaction)| {
                FullTransaction::from_zebra(transaction, txids.get(index).cloned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let height = block
            .coinbase_height()
            .map(|height| i32::try_from(height.0))
            .transpose()?
            .ok_or_else(|| ParseError::InvalidData("missing block coinbase height".to_string()))?;

        Ok((&data[consumed..], Self { hdr, vtx, height }))
    }
}

impl FullBlock {
    /// Returns the full block header.
    pub fn header(&self) -> FullBlockHeader {
        self.hdr.clone()
    }

    /// Returns the transactions held in the block.
    pub fn transactions(&self) -> Vec<super::transaction::FullTransaction> {
        self.vtx.clone()
    }

    /// Returns the block height.
    pub fn height(&self) -> i32 {
        self.height
    }

    /// Returns the Orchard `authDataRoot` of the block, taken from the coinbase transaction's anchorOrchard field.
    ///
    /// If the coinbase transaction includes an Orchard bundle, this is the root of the Orchard commitment tree
    /// after applying all Orchard actions in the block.
    ///
    /// Returns `Some(Vec<u8>)` if present, else `None`.
    pub fn auth_data_root(&self) -> Option<Vec<u8>> {
        self.vtx.first().and_then(|tx| tx.anchor_orchard())
    }

    /// Decodes a hex encoded zcash full block into a FullBlock struct.
    pub fn parse_from_hex(data: &[u8], txid: Option<Vec<Vec<u8>>>) -> Result<Self, ParseError> {
        let (remaining_data, full_block) = Self::parse_from_slice(data, txid, None)?;
        if !remaining_data.is_empty() {
            return Err(ParseError::InvalidData(format!(
                "Error decoding full block - {} bytes of Remaining data. Compact Block Created: ({:?})",
                remaining_data.len(),
                full_block
                    .clone()
                    .into_compact_block(0, 0, 0, PoolTypeFilter::includes_all())
            )));
        }
        Ok(full_block)
    }

    /// Turns this Block into a Compact Block according to the Lightclient protocol [ZIP-307](https://zips.z.cash/zip-0307)
    /// callers can choose which pools to include in this compact block by specifying a
    /// `PoolTypeFilter` accordingly.
    pub fn into_compact_block(
        self,
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
        ironwood_commitment_tree_size: u32,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlock, ParseError> {
        let vtx = self
            .vtx
            .into_iter()
            .enumerate()
            .filter_map(
                |(index, tx)| match tx.to_compact_tx(Some(index as u64), &pool_types) {
                    Ok(compact_tx) => {
                        if !compact_tx.vin.is_empty()
                            || !compact_tx.vout.is_empty()
                            || !compact_tx.spends.is_empty()
                            || !compact_tx.outputs.is_empty()
                            || !compact_tx.actions.is_empty()
                            || !compact_tx.ironwood_actions.is_empty()
                        {
                            Some(Ok(compact_tx))
                        } else {
                            None
                        }
                    }
                    Err(parse_error) => Some(Err(parse_error)),
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CompactBlock {
            proto_version: 1,
            height: self.height as u64,
            hash: self.hdr.cached_hash.clone(),
            prev_hash: self.hdr.hash_prev_block(),
            time: self.hdr.time(),
            header: Vec::new(),
            vtx,
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size,
                orchard_commitment_tree_size,
                ironwood_commitment_tree_size,
            }),
        })
    }
}
