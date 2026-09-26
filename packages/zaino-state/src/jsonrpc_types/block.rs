use zebra_chain::{
    block::{self, Height, SerializedBlock},
    transaction,
    work::{difficulty::CompactDifficulty, equihash::Solution},
};

use super::balance::{BlockchainValuePoolBalances, GetBlockchainInfoBalance};
use super::hex::opthex;
use super::transaction::TransactionObject;

/// A response to a `getblock` request: the raw block at verbosity 0, an object otherwise.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum GetBlock {
    /// The request block, hex-encoded.
    Raw(#[serde(with = "hex")] SerializedBlock),
    /// The block object.
    Object(Box<BlockObject>),
}

/// A block object returned by the `getblock` request.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BlockObject {
    /// The hash of the requested block.
    #[serde(with = "hex")]
    pub hash: block::Hash,

    /// The number of confirmations of this block in the best chain, or -1 if it is not in the best chain.
    pub confirmations: i64,

    /// The block size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,

    /// The height of the requested block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<Height>,

    /// The version field of the requested block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,

    /// The merkle root of the requested block.
    #[serde(with = "opthex", rename = "merkleroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merkle_root: Option<block::merkle::Root>,

    /// The blockcommitments field of the requested block, whose meaning depends on the network upgrade.
    #[serde(with = "opthex", rename = "blockcommitments")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_commitments: Option<[u8; 32]>,

    /// The root of the Sapling commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalsaplingroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<[u8; 32]>,

    /// The root of the Orchard commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalorchardroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_orchard_root: Option<[u8; 32]>,

    /// The number of transactions in this block.
    #[serde(rename = "nTx")]
    pub n_tx: usize,

    /// The transactions in block order, as ids at verbosity 1 and as objects at verbosity 2.
    pub tx: Vec<GetBlockTransaction>,

    /// The block time, in seconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,

    /// The nonce of the requested block header.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<[u8; 32]>,

    /// The Equihash solution in the requested block header, a field zcashd does not document.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution: Option<Solution>,

    /// The difficulty threshold of the requested block header displayed in compact form.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<CompactDifficulty>,

    /// The block's difficulty as a multiple of the network's minimum difficulty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<f64>,

    /// Chain supply balance.
    #[serde(rename = "chainSupply")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_supply: Option<GetBlockchainInfoBalance>,

    /// Value pool balances.
    #[serde(rename = "valuePools")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_pools: Option<BlockchainValuePoolBalances>,

    /// Information about the note commitment trees.
    pub trees: GetBlockTrees,

    /// The previous block hash of the requested block header.
    #[serde(rename = "previousblockhash", skip_serializing_if = "Option::is_none")]
    #[serde(with = "opthex")]
    pub previous_block_hash: Option<block::Hash>,

    /// The next block hash after the requested block header.
    #[serde(rename = "nextblockhash", skip_serializing_if = "Option::is_none")]
    #[serde(with = "opthex")]
    pub next_block_hash: Option<block::Hash>,
}

/// A `getblock` transaction entry: its id at verbosity 1, its full object at verbosity 2.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum GetBlockTransaction {
    /// The transaction hash, hex-encoded.
    Hash(#[serde(with = "hex")] transaction::Hash),
    /// The transaction object.
    Object(Box<TransactionObject>),
}

/// A response to `getbestblockhash` or `getblockhash`: the block hash as display-order hex.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct GetBlockHash(#[serde(with = "hex")] block::Hash);

impl GetBlockHash {
    /// Wraps a block hash as the response.
    pub fn new(hash: block::Hash) -> Self {
        GetBlockHash(hash)
    }
}

/// The note commitment tree sizes of each shielded pool, omitting any pool whose tree is empty.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct GetBlockTrees {
    #[serde(skip_serializing_if = "TreeSize::is_empty")]
    sapling: TreeSize,
    #[serde(skip_serializing_if = "TreeSize::is_empty")]
    orchard: TreeSize,
    #[serde(skip_serializing_if = "TreeSize::is_empty")]
    ironwood: TreeSize,
}

impl GetBlockTrees {
    /// Constructs the tree sizes from each pool's note count.
    pub fn new(sapling: u64, orchard: u64, ironwood: u64) -> Self {
        GetBlockTrees {
            sapling: TreeSize { size: sapling },
            orchard: TreeSize { size: orchard },
            ironwood: TreeSize { size: ironwood },
        }
    }
}

/// One pool's note commitment tree size.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
struct TreeSize {
    size: u64,
}

impl TreeSize {
    /// Whether the tree holds no notes.
    fn is_empty(&self) -> bool {
        self.size == 0
    }
}
