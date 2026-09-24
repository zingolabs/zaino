use derive_getters::Getters;
use derive_new::new;
use zebra_chain::{
    block::{self, Height, SerializedBlock},
    transaction,
    work::{difficulty::CompactDifficulty, equihash::Solution},
};

use super::balance::{BlockchainValuePoolBalances, GetBlockchainInfoBalance};
use super::hex::opthex;
use super::transaction::TransactionObject;

/// A response to a `getblock` request: the raw block at verbosity 0, an object otherwise.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum GetBlock {
    /// The request block, hex-encoded.
    Raw(#[serde(with = "hex")] SerializedBlock),
    /// The block object.
    Object(Box<BlockObject>),
}

/// A block object returned by the `getblock` request.
#[allow(clippy::too_many_arguments)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct BlockObject {
    /// The hash of the requested block.
    #[getter(copy)]
    #[serde(with = "hex")]
    hash: block::Hash,

    /// The number of confirmations of this block in the best chain, or -1 if it is not in the best chain.
    confirmations: i64,

    /// The block size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    size: Option<i64>,

    /// The height of the requested block.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    height: Option<Height>,

    /// The version field of the requested block.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    version: Option<u32>,

    /// The merkle root of the requested block.
    #[serde(with = "opthex", rename = "merkleroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    merkle_root: Option<block::merkle::Root>,

    /// The blockcommitments field of the requested block, whose meaning depends on the network upgrade.
    #[serde(with = "opthex", rename = "blockcommitments")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    block_commitments: Option<[u8; 32]>,

    /// The root of the Sapling commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalsaplingroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    final_sapling_root: Option<[u8; 32]>,

    /// The root of the Orchard commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalorchardroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    final_orchard_root: Option<[u8; 32]>,

    /// The number of transactions in this block.
    #[serde(rename = "nTx")]
    n_tx: usize,

    /// The transactions in block order, as ids at verbosity 1 and as objects at verbosity 2.
    tx: Vec<GetBlockTransaction>,

    /// The block time, in seconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    time: Option<i64>,

    /// The nonce of the requested block header.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    nonce: Option<[u8; 32]>,

    /// The Equihash solution in the requested block header, a field zcashd does not document.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    solution: Option<Solution>,

    /// The difficulty threshold of the requested block header displayed in compact form.
    #[serde(with = "opthex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    bits: Option<CompactDifficulty>,

    /// The block's difficulty as a multiple of the network's minimum difficulty.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    difficulty: Option<f64>,

    /// Chain supply balance.
    #[serde(rename = "chainSupply")]
    #[serde(skip_serializing_if = "Option::is_none")]
    chain_supply: Option<GetBlockchainInfoBalance>,

    /// Value pool balances.
    #[serde(rename = "valuePools")]
    #[serde(skip_serializing_if = "Option::is_none")]
    value_pools: Option<BlockchainValuePoolBalances>,

    /// Information about the note commitment trees.
    #[getter(copy)]
    trees: GetBlockTrees,

    /// The previous block hash of the requested block header.
    #[serde(rename = "previousblockhash", skip_serializing_if = "Option::is_none")]
    #[serde(with = "opthex")]
    #[getter(copy)]
    previous_block_hash: Option<block::Hash>,

    /// The next block hash after the requested block header.
    #[serde(rename = "nextblockhash", skip_serializing_if = "Option::is_none")]
    #[serde(with = "opthex")]
    #[getter(copy)]
    next_block_hash: Option<block::Hash>,
}

/// A `getblock` transaction entry: its id at verbosity 1, its full object at verbosity 2.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum GetBlockTransaction {
    /// The transaction hash, hex-encoded.
    Hash(#[serde(with = "hex")] transaction::Hash),
    /// The transaction object.
    Object(Box<TransactionObject>),
}

/// A response to `getbestblockhash` or `getblockhash`: the block hash as display-order hex.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct GetBlockHash(#[serde(with = "hex")] block::Hash);

impl GetBlockHash {
    /// Wraps a block hash as the response.
    pub fn new(hash: block::Hash) -> Self {
        GetBlockHash(hash)
    }

    /// Returns the block hash.
    pub fn hash(&self) -> block::Hash {
        self.0
    }
}

/// The note commitment tree sizes of each shielded pool, omitting any pool whose tree is empty.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBlockTrees {
    #[serde(skip_serializing_if = "SaplingTrees::is_empty")]
    sapling: SaplingTrees,
    #[serde(skip_serializing_if = "OrchardTrees::is_empty")]
    orchard: OrchardTrees,
    #[serde(default, skip_serializing_if = "IronwoodTrees::is_empty")]
    ironwood: IronwoodTrees,
}

impl GetBlockTrees {
    /// Constructs the tree sizes from each pool's note count.
    pub fn new(sapling: u64, orchard: u64, ironwood: u64) -> Self {
        GetBlockTrees {
            sapling: SaplingTrees { size: sapling },
            orchard: OrchardTrees { size: orchard },
            ironwood: IronwoodTrees { size: ironwood },
        }
    }

    /// Returns the Sapling tree size.
    pub fn sapling(self) -> u64 {
        self.sapling.size
    }

    /// Returns the Orchard tree size.
    pub fn orchard(self) -> u64 {
        self.orchard.size
    }

    /// Returns the Ironwood tree size.
    pub fn ironwood(self) -> u64 {
        self.ironwood.size
    }
}

/// Sapling note commitment tree information.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SaplingTrees {
    size: u64,
}

impl SaplingTrees {
    /// Whether the tree holds no notes.
    fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// Orchard note commitment tree information.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OrchardTrees {
    size: u64,
}

impl OrchardTrees {
    /// Whether the tree holds no notes.
    fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// Ironwood note commitment tree information, in the Orchard tree's shape.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IronwoodTrees {
    size: u64,
}

impl IronwoodTrees {
    /// Whether the tree holds no notes.
    fn is_empty(&self) -> bool {
        self.size == 0
    }
}
