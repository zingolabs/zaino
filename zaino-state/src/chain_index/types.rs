//! Holds chain and block structs used internally by the ChainIndex.
//!
//! Held here to ensure serialisation consistency for ZainoDB.

use dcbor::{CBORCase, CBORTagged, CBORTaggedDecodable, CBORTaggedEncodable, Tag, CBOR};
use hex::{FromHex, ToHex};
use primitive_types::U256;
use std::fmt;

/// DCBOR serialisation schema tags.
///
/// All structs in this module should be included in this enum.
///
/// Items should use the range 510 - 599 (500 - 509 are reserved for database validation.).
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborTag {
    ChainBlock = 510,
    BlockIndex = 511,
    Hash = 512,
    Height = 513,
    Index = 514,
    ChainWork = 515,
    BlockData = 516,
    CommitmentTreeRoots = 517,
    CommitmentTreeSizes = 518,
    TxData = 519,
    TransparentCompactTx = 520,
    TxInCompact = 521,
    ScriptType = 522,
    TxOutCompact = 523,
    SaplingCompactTx = 524,
    CompactSaplingSpend = 525,
    CompactSaplingOutput = 526,
    CompactOrchardAction = 527,
    SpentOutpoint = 528,
    ShardRoot = 529,
}

impl CborTag {
    pub fn tag(self) -> dcbor::Tag {
        dcbor::Tag::with_value(self as u64)
    }
}

/// Represents the indexing data of a single compact Zcash block used internally by Zaino.
/// Provides efficient indexing for blockchain state queries and updates.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct ChainBlock {
    /// Metadata and indexing information for this block.
    index: BlockIndex,
    /// Essential header and metadata information for the block.
    data: BlockData,
    /// Compact representations of transactions in this block.
    tx: Vec<TxData>,
    /// Explicitly recorded UTXOs spent in this block, speeding up reorgs and delta indexing.
    spent_outpoints: Vec<SpentOutpoint>,
}

impl ChainBlock {
    /// Creates a new `ChainBlock`.
    pub fn new(
        index: BlockIndex,
        data: BlockData,
        tx: Vec<TxData>,
        spent_outpoints: Vec<SpentOutpoint>,
    ) -> Self {
        Self {
            index,
            data,
            tx,
            spent_outpoints,
        }
    }

    /// Returns a reference to the block index metadata.
    pub fn index(&self) -> &BlockIndex {
        &self.index
    }

    /// Returns a reference to the header and auxiliary block data.
    pub fn data(&self) -> &BlockData {
        &self.data
    }

    /// Returns a reference to the compact transactions in this block.
    pub fn transactions(&self) -> &[TxData] {
        &self.tx
    }

    /// Returns a reference to the spent transparent outpoints.
    pub fn spent_outpoints(&self) -> &[SpentOutpoint] {
        &self.spent_outpoints
    }

    /// Returns the block hash.
    pub fn hash(&self) -> &Hash {
        self.index.hash()
    }

    /// Returns the block height if available.
    pub fn height(&self) -> Option<Height> {
        self.index.height()
    }

    /// Returns true if this block is part of the best chain.
    pub fn is_on_best_chain(&self) -> bool {
        self.index.is_on_best_chain()
    }

    /// Returns the cumulative chainwork.
    pub fn chainwork(&self) -> &ChainWork {
        self.index.chainwork()
    }

    /// Returns the raw work value (targeted work contribution).
    pub fn work(&self) -> U256 {
        self.data.work()
    }

    /// Converts this `ChainBlock` into a CompactBlock protobuf message using proto v4 format.
    pub fn to_compact_block(&self) -> zaino_proto::proto::compact_formats::CompactBlock {
        // NOTE: Returns u64::MAX if the block is not in the best chain.
        let height: u64 = self.height().map(|h| h.0.into()).unwrap_or(u64::MAX);

        let hash = self.hash().0.to_vec();
        let prev_hash = self.index().parent_hash().0.to_vec();

        let vtx: Vec<zaino_proto::proto::compact_formats::CompactTx> = self
            .transactions()
            .iter()
            .map(|tx| tx.to_compact_tx(None))
            .collect();

        let sapling_commitment_tree_size = self.data().commitment_tree_size().sapling();
        let orchard_commitment_tree_size = self.data().commitment_tree_size().orchard();

        zaino_proto::proto::compact_formats::CompactBlock {
            proto_version: 4,
            height,
            hash,
            prev_hash,
            time: self.data().time() as u32,
            // Header not currently used by CompactBlocks.
            header: vec![],
            vtx,
            chain_metadata: Some(zaino_proto::proto::compact_formats::ChainMetadata {
                sapling_commitment_tree_size,
                orchard_commitment_tree_size,
            }),
        }
    }
}

/// TryFrom inputs:
/// - FullBlock:
///   - Holds block data.
/// - parent_block_chain_work:
///   - Used to calculate cumulative chain work.
/// - Final sapling root:
///  - Must be fetched from separate RPC.
/// - Final orchard root:
///  - Must be fetched from separate RPC.
/// - parent_block_sapling_tree_size:
///   - Used to calculate sapling tree size.
/// - parent_block_orchard_tree_size:
///   - Used to calculate sapling tree size.
impl
    TryFrom<(
        zaino_fetch::chain::block::FullBlock,
        ChainWork,
        [u8; 32],
        [u8; 32],
        u32,
        u32,
    )> for ChainBlock
{
    type Error = String;

    fn try_from(
        (
            full_block,
            parent_chainwork,
            final_sapling_root,
            final_orchard_root,
            parent_sapling_size,
            parent_orchard_size,
        ): (
            zaino_fetch::chain::block::FullBlock,
            ChainWork,
            [u8; 32],
            [u8; 32],
            u32,
            u32,
        ),
    ) -> Result<Self, Self::Error> {
        // --- Block Header Info ---
        let header = full_block.header();
        let height = Height::try_from(full_block.height() as u32)
            .map_err(|e| format!("Invalid block height: {e}"))?;

        let hash: [u8; 32] = header
            .cached_hash()
            .try_into()
            .map_err(|_| "Block hash must be 32 bytes")?;
        let parent_hash: [u8; 32] = header
            .hash_prev_block()
            .try_into()
            .map_err(|_| "Parent block hash must be 32 bytes")?;

        let n_bits_bytes = header.n_bits_bytes();
        if n_bits_bytes.len() != 4 {
            return Err("nBits must be 4 bytes".to_string());
        }
        let bits = u32::from_le_bytes(n_bits_bytes.try_into().unwrap());

        // --- Fetch AuthDataRoot ---
        let auth_data_root = full_block
            .auth_data_root()
            .and_then(|v| v.try_into().ok())
            .unwrap_or([0u8; 32]);

        let mut sapling_note_count = 0;
        let mut orchard_note_count = 0;

        // --- Convert transactions ---
        let full_transactions = full_block.transactions();
        let mut tx = Vec::with_capacity(full_transactions.len());
        let mut spent_outpoints = Vec::new();

        for (i, ftx) in full_transactions.into_iter().enumerate() {
            let txdata = TxData::try_from((i as u64, ftx))
                .map_err(|e| format!("TxData conversion failed at index {}: {e}", i))?;

            sapling_note_count += txdata.sapling().outputs().len();
            orchard_note_count += txdata.orchard().len();

            for input in txdata.transparent().inputs() {
                spent_outpoints.push(SpentOutpoint::new(
                    *input.prevout_txid(),
                    input.prevout_index(),
                ));
            }

            tx.push(txdata);
        }

        // --- Compute commitment trees ---
        let sapling_root = final_sapling_root;
        let orchard_root = final_orchard_root;

        let commitment_tree_roots = CommitmentTreeRoots::new(sapling_root, orchard_root);

        let commitment_tree_size = CommitmentTreeSizes::new(
            parent_sapling_size + sapling_note_count as u32,
            parent_orchard_size + orchard_note_count as u32,
        );

        // --- Compute chainwork ---
        let block_data = BlockData::new(
            header.version() as u32,
            header.time() as i64,
            header.hash_merkle_root().try_into().unwrap(),
            auth_data_root,
            commitment_tree_roots,
            commitment_tree_size,
            bits,
        );
        let chainwork = parent_chainwork.add(&ChainWork::from(block_data.work()));

        // --- Final index and block data ---
        let index = BlockIndex::new(
            Hash::from(hash),
            Hash::from(parent_hash),
            chainwork,
            Some(height),
        );

        Ok(ChainBlock::new(index, block_data, tx, spent_outpoints))
    }
}

impl CBORTagged for ChainBlock {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::ChainBlock.tag()]
    }
}

impl CBORTaggedEncodable for ChainBlock {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            self.index.tagged_cbor(),
            self.data.tagged_cbor(),
            CBOR::from(self.tx.iter().map(|t| t.tagged_cbor()).collect::<Vec<_>>()),
            CBOR::from(
                self.spent_outpoints
                    .iter()
                    .map(|s| s.tagged_cbor())
                    .collect::<Vec<_>>(),
            ),
        ])
    }
}

impl CBORTaggedDecodable for ChainBlock {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 4 => {
                let index = BlockIndex::try_from(arr[0].clone())?;
                let data = BlockData::try_from(arr[1].clone())?;

                let tx_arr = match arr[2].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };
                let tx = tx_arr
                    .into_iter()
                    .map(TxData::try_from)
                    .collect::<Result<_, _>>()?;

                let spent_arr = match arr[3].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };
                let spent_outpoints = spent_arr
                    .into_iter()
                    .map(SpentOutpoint::try_from)
                    .collect::<Result<_, _>>()?;

                Ok(Self {
                    index,
                    data,
                    tx,
                    spent_outpoints,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for ChainBlock {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Metadata about the block used to identify and navigate the blockchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockIndex {
    /// The hash identifying this block uniquely.
    hash: Hash,
    /// The hash of this block's parent block (previous block in chain).
    parent_hash: Hash,
    /// The cumulative proof-of-work of the blockchain up to this block, used for chain selection.
    chainwork: ChainWork,
    /// The height of this block if it's in the current best chain. None if it's part of a fork.
    height: Option<Height>,
}

impl BlockIndex {
    /// Constructs a new `BlockIndex`.
    pub fn new(
        hash: Hash,
        parent_hash: Hash,
        chainwork: ChainWork,
        height: Option<Height>,
    ) -> Self {
        Self {
            hash,
            parent_hash,
            chainwork,
            height,
        }
    }

    /// Returns the hash of this block.
    pub fn hash(&self) -> &Hash {
        &self.hash
    }

    /// Returns the hash of the parent block.
    pub fn parent_hash(&self) -> &Hash {
        &self.parent_hash
    }

    /// Returns the cumulative chainwork up to this block.
    pub fn chainwork(&self) -> &ChainWork {
        &self.chainwork
    }

    /// Returns the height of this block if it’s part of the best chain.
    pub fn height(&self) -> Option<Height> {
        self.height
    }

    /// Returns true if this block is part of the best chain.
    pub fn is_on_best_chain(&self) -> bool {
        self.height.is_some()
    }
}

impl CBORTagged for BlockIndex {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::BlockIndex.tag()]
    }
}

impl CBORTaggedEncodable for BlockIndex {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            self.hash.untagged_cbor(),
            self.parent_hash.untagged_cbor(),
            self.chainwork.untagged_cbor(),
            match self.height {
                Some(h) => h.untagged_cbor(),
                None => CBOR::null(), // ✅ CORRECT for dCBOR
            },
        ])
    }
}

impl CBORTaggedDecodable for BlockIndex {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 4 => {
                let hash = Hash::from_untagged_cbor(arr[0].clone())?;
                let parent_hash = Hash::from_untagged_cbor(arr[1].clone())?;
                let chainwork = ChainWork::from_untagged_cbor(arr[2].clone())?;

                let height = if arr[3].is_null() {
                    None
                } else {
                    Some(Height::from_untagged_cbor(arr[3].clone())?)
                };

                Ok(BlockIndex {
                    hash,
                    parent_hash,
                    chainwork,
                    height,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for BlockIndex {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Block hash (SHA256d hash of the block header).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Hash([u8; 32]);

impl Hash {
    /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
    pub fn bytes_in_display_order(&self) -> [u8; 32] {
        let mut reversed_bytes = self.0;
        reversed_bytes.reverse();
        reversed_bytes
    }

    /// Convert bytes in big-endian byte-order into a [`block::Hash`](crate::block::Hash).
    pub fn from_bytes_in_display_order(bytes_in_display_order: &[u8; 32]) -> Hash {
        let mut internal_byte_order = *bytes_in_display_order;
        internal_byte_order.reverse();

        Hash(internal_byte_order)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl ToHex for &Hash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl ToHex for Hash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl FromHex for Hash {
    type Error = <[u8; 32] as FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hash = <[u8; 32]>::from_hex(hex)?;

        Ok(Self::from_bytes_in_display_order(&hash))
    }
}

impl From<[u8; 32]> for Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Hash(bytes)
    }
}

impl From<Hash> for [u8; 32] {
    fn from(hash: Hash) -> Self {
        hash.0
    }
}

impl From<Hash> for zebra_chain::block::Hash {
    fn from(h: Hash) -> Self {
        zebra_chain::block::Hash(h.0)
    }
}

impl From<zebra_chain::block::Hash> for Hash {
    fn from(h: zebra_chain::block::Hash) -> Self {
        Hash(h.0)
    }
}

impl From<Hash> for zcash_primitives::block::BlockHash {
    fn from(h: Hash) -> Self {
        // Convert to display order (big-endian)
        zcash_primitives::block::BlockHash(h.bytes_in_display_order())
    }
}

impl From<zcash_primitives::block::BlockHash> for Hash {
    fn from(h: zcash_primitives::block::BlockHash) -> Self {
        Hash::from_bytes_in_display_order(&h.0)
    }
}

impl CBORTagged for Hash {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::Hash.tag()]
    }
}

impl CBORTaggedEncodable for Hash {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(&self.0[..])
    }
}

impl CBORTaggedDecodable for Hash {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        let bytes: Vec<u8> = cbor.try_into()?;
        let array: [u8; 32] = bytes.try_into().map_err(|_| dcbor::Error::WrongType)?;
        Ok(Hash(array))
    }
}

impl TryFrom<CBOR> for Hash {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Block height.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Height(u32);

impl TryFrom<u32> for Height {
    type Error = &'static str;

    fn try_from(height: u32) -> Result<Self, Self::Error> {
        // Zebra enforces Height <= 2^31 - 1
        if height <= zebra_chain::block::Height::MAX.0 {
            Ok(Self(height))
        } else {
            Err("height must be ≤ 2^31 - 1")
        }
    }
}

impl From<Height> for u32 {
    fn from(h: Height) -> Self {
        h.0
    }
}

impl std::fmt::Display for Height {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Height {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.parse::<u32>().map_err(|_| "invalid u32")?;
        Self::try_from(h)
    }
}

impl From<Height> for zebra_chain::block::Height {
    fn from(h: Height) -> Self {
        zebra_chain::block::Height(h.0)
    }
}

impl TryFrom<zebra_chain::block::Height> for Height {
    type Error = &'static str;

    fn try_from(h: zebra_chain::block::Height) -> Result<Self, Self::Error> {
        Height::try_from(h.0)
    }
}

impl From<Height> for zcash_protocol::consensus::BlockHeight {
    fn from(h: Height) -> Self {
        zcash_protocol::consensus::BlockHeight::from(h.0)
    }
}

impl TryFrom<zcash_protocol::consensus::BlockHeight> for Height {
    type Error = &'static str;

    fn try_from(h: zcash_protocol::consensus::BlockHeight) -> Result<Self, Self::Error> {
        Height::try_from(u32::from(h))
    }
}

impl CBORTagged for Height {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::Height.tag()]
    }
}

impl CBORTaggedEncodable for Height {
    fn untagged_cbor(&self) -> CBOR {
        self.0.into()
    }
}

impl CBORTaggedDecodable for Height {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        Ok(Height(cbor.try_into()?))
    }
}

impl TryFrom<CBOR> for Height {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Numerical index of subtree / shard roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Index(pub u32);

impl CBORTagged for Index {
    fn cbor_tags() -> Vec<dcbor::Tag> {
        vec![CborTag::Index.tag()]
    }
}

impl CBORTaggedEncodable for Index {
    fn untagged_cbor(&self) -> CBOR {
        self.0.into()
    }
}

impl CBORTaggedDecodable for Index {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        Ok(Index(cbor.try_into()?))
    }
}

impl TryFrom<CBOR> for Index {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Cumulative proof-of-work of the chain,
/// stored as a **big-endian** 256-bit unsigned integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct ChainWork([u8; 32]);

impl ChainWork {
    ///Returns ChainWork as a U256.
    pub fn to_u256(&self) -> U256 {
        U256::from_big_endian(&self.0)
    }

    /// Builds a ChainWork from a U256.
    pub fn from_u256(value: U256) -> Self {
        let buf: [u8; 32] = value.to_big_endian();
        ChainWork(buf)
    }

    /// Adds 2 ChainWorks.
    pub fn add(&self, other: &Self) -> Self {
        Self::from_u256(self.to_u256() + other.to_u256())
    }

    /// Subtract one ChainWork from another.
    pub fn sub(&self, other: &Self) -> Self {
        Self::from_u256(self.to_u256() - other.to_u256())
    }

    /// Returns ChainWork bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<U256> for ChainWork {
    fn from(value: U256) -> Self {
        Self::from_u256(value)
    }
}

impl From<ChainWork> for U256 {
    fn from(value: ChainWork) -> Self {
        value.to_u256()
    }
}

impl fmt::Display for ChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_u256().fmt(f)
    }
}

impl CBORTagged for ChainWork {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::ChainWork.tag()]
    }
}

impl CBORTaggedEncodable for ChainWork {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(&self.0[..])
    }
}

impl CBORTaggedDecodable for ChainWork {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        let bytes: Vec<u8> = cbor.try_into()?;
        let array: [u8; 32] = bytes.try_into().map_err(|_| dcbor::Error::WrongType)?;
        Ok(ChainWork(array))
    }
}

impl TryFrom<CBOR> for ChainWork {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Essential block header fields required for quick indexing and client-serving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockData {
    /// Version number of the block format (protocol upgrades).
    version: u32,
    /// Unix timestamp of when the block was mined (seconds since epoch).
    time: i64,
    /// Merkle root hash of all transaction IDs in the block (used for quick tx inclusion proofs).
    merkle_root: [u8; 32],
    /// Digest representing the block-commitments Merkle root (commitment to note states).
    ///
    /// [`hashAuthDataRoot`]
    auth_data_root: [u8; 32],
    /// Roots of the Sapling and Orchard note-commitment trees (for shielded tx verification).
    commitment_tree_roots: CommitmentTreeRoots,
    /// Number of leaves in Sapling/Orchard note commitment trees at this block.
    commitment_tree_size: CommitmentTreeSizes,
    /// Compact difficulty target used for proof-of-work and difficulty calculation.
    bits: u32,
}

impl BlockData {
    /// Creates a new  BlockData instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        time: i64,
        merkle_root: [u8; 32],
        auth_data_root: [u8; 32],
        commitment_tree_roots: CommitmentTreeRoots,
        commitment_tree_size: CommitmentTreeSizes,
        bits: u32,
    ) -> Self {
        Self {
            version,
            time,
            merkle_root,
            auth_data_root,
            commitment_tree_roots,
            commitment_tree_size,
            bits,
        }
    }

    /// Returns block Version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Returns block time.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Returns block merkle root.
    pub fn merkle_root(&self) -> &[u8; 32] {
        &self.merkle_root
    }

    /// Returns blocks authDataRoot.
    pub fn auth_data_root(&self) -> &[u8; 32] {
        &self.auth_data_root
    }

    /// Returns commitment tree roots.
    pub fn commitment_tree_roots(&self) -> &CommitmentTreeRoots {
        &self.commitment_tree_roots
    }

    /// Returns commitment tree sizes.
    pub fn commitment_tree_size(&self) -> &CommitmentTreeSizes {
        &self.commitment_tree_size
    }

    /// Returns nbits.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Converts compact bits field into the full target as a 256-bit integer.
    pub fn target(&self) -> U256 {
        Self::compact_to_target_u256(self.bits)
    }

    /// Returns the block work as 2^256 / (target + 1)
    pub fn work(&self) -> U256 {
        let target = self.target();
        if target.is_zero() {
            U256::zero()
        } else {
            (U256::one() << 256) / (target + 1)
        }
    }

    /// Returns difficulty as ratio of the genesis target to this block's target.
    pub fn difficulty(&self) -> f64 {
        let max_target = Self::compact_to_target_u256(0x1d00ffff); // Zcash genesis
        let target = self.target();
        Self::u256_to_f64(max_target) / Self::u256_to_f64(target)
    }

    /// Used to convert bits to target.
    fn compact_to_target_u256(bits: u32) -> U256 {
        let exponent = (bits >> 24) as usize;
        let mantissa = bits & 0x007fffff;

        if exponent <= 3 {
            U256::from(mantissa) >> (8 * (3 - exponent))
        } else {
            U256::from(mantissa) << (8 * (exponent - 3))
        }
    }

    /// Converts a `U256` to `f64` lossily (sufficient for difficulty comparison).
    fn u256_to_f64(value: U256) -> f64 {
        let mut result = 0.0f64;
        for (i, word) in value.0.iter().enumerate() {
            result += (*word as f64) * 2f64.powi(64 * i as i32);
        }
        result
    }
}

impl CBORTagged for BlockData {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::BlockData.tag()]
    }
}

impl CBORTaggedEncodable for BlockData {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(self.version),
            CBOR::from(self.time),
            CBOR::from(&self.merkle_root[..]),
            CBOR::from(&self.auth_data_root[..]),
            self.commitment_tree_roots.untagged_cbor(),
            self.commitment_tree_size.untagged_cbor(),
            CBOR::from(self.bits),
        ])
    }
}

impl CBORTaggedDecodable for BlockData {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 7 => {
                let version = arr[0].clone().try_into()?;
                let time = arr[1].clone().try_into()?;
                let merkle_root_vec: Vec<u8> = arr[2].clone().try_into()?;
                let commitment_digest_vec: Vec<u8> = arr[3].clone().try_into()?;
                let commitment_tree_roots =
                    CommitmentTreeRoots::from_untagged_cbor(arr[4].clone())?;
                let commitment_tree_size = CommitmentTreeSizes::from_untagged_cbor(arr[5].clone())?;
                let bits = arr[6].clone().try_into()?;

                Ok(Self {
                    version,
                    time,
                    merkle_root: merkle_root_vec
                        .try_into()
                        .map_err(|_| dcbor::Error::WrongType)?,
                    auth_data_root: commitment_digest_vec
                        .try_into()
                        .map_err(|_| dcbor::Error::WrongType)?,
                    commitment_tree_roots,
                    commitment_tree_size,
                    bits,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for BlockData {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Commitment tree roots for shielded transactions, enabling shielded wallet synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CommitmentTreeRoots {
    /// Sapling note-commitment tree root (anchor) at this block.
    sapling: [u8; 32],
    /// Orchard note-commitment tree root at this block.
    orchard: [u8; 32],
}

impl CommitmentTreeRoots {
    /// Reutns a new CommitmentTreeRoots instance.
    pub fn new(sapling: [u8; 32], orchard: [u8; 32]) -> Self {
        Self { sapling, orchard }
    }

    /// Returns sapling commitment tree root.
    pub fn sapling(&self) -> &[u8; 32] {
        &self.sapling
    }

    /// returns orchard commitment tree root.
    pub fn orchard(&self) -> &[u8; 32] {
        &self.orchard
    }
}

impl CBORTagged for CommitmentTreeRoots {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::CommitmentTreeRoots.tag()]
    }
}

impl CBORTaggedEncodable for CommitmentTreeRoots {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(&self.sapling[..]),
            CBOR::from(&self.orchard[..]),
        ])
    }
}

impl CBORTaggedDecodable for CommitmentTreeRoots {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let sapling: Vec<u8> = arr[0].clone().try_into()?;
                let orchard: Vec<u8> = arr[1].clone().try_into()?;
                Ok(Self {
                    sapling: sapling.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    orchard: orchard.try_into().map_err(|_| dcbor::Error::WrongType)?,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for CommitmentTreeRoots {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Sizes of commitment trees, indicating total number of shielded notes created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CommitmentTreeSizes {
    /// Total notes in Sapling commitment tree.
    sapling: u32,
    /// Total notes in Orchard commitment tree.
    orchard: u32,
}

impl CommitmentTreeSizes {
    /// Creates a new CompactSaplingSizes instance.
    pub fn new(sapling: u32, orchard: u32) -> Self {
        Self { sapling, orchard }
    }

    /// Returns sapling commitment tree size
    pub fn sapling(&self) -> u32 {
        self.sapling
    }

    /// Returns orchard commitment tree size
    pub fn orchard(&self) -> u32 {
        self.orchard
    }
}

impl CBORTagged for CommitmentTreeSizes {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::CommitmentTreeSizes.tag()]
    }
}

impl CBORTaggedEncodable for CommitmentTreeSizes {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![CBOR::from(self.sapling), CBOR::from(self.orchard)])
    }
}

impl CBORTaggedDecodable for CommitmentTreeSizes {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let sapling: u32 = arr[0].clone().try_into()?;
                let orchard: u32 = arr[1].clone().try_into()?;
                Ok(Self { sapling, orchard })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for CommitmentTreeSizes {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact indexed representation of a transaction within a block, supporting quick queries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxData {
    /// The index (position) of this transaction within its block (0-based).
    index: u64,
    /// Unique identifier (hash) of the transaction, used for lookup and indexing.
    txid: [u8; 32],
    /// Sapling and Orchard value balances, (Sapling, Orchard).
    value_balances: (Option<i64>, Option<i64>),
    /// Compact representation of transparent inputs/outputs in the transaction.
    transparent: TransparentCompactTx,
    /// Compact representation of Sapling shielded data.
    sapling: SaplingCompactTx,
    /// Compact representation of Orchard actions (shielded pool transactions).
    orchard: Vec<CompactOrchardAction>,
}

impl TxData {
    /// Creates a new TxData instance.
    pub fn new(
        index: u64,
        txid: [u8; 32],
        value_balances: (Option<i64>, Option<i64>),
        transparent: TransparentCompactTx,
        sapling: SaplingCompactTx,
        orchard: Vec<CompactOrchardAction>,
    ) -> Self {
        Self {
            index,
            txid,
            value_balances,
            transparent,
            sapling,
            orchard,
        }
    }

    /// Returns transactions index within block.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns transaction ID.
    pub fn txid(&self) -> &[u8; 32] {
        &self.txid
    }

    /// Returns sapling and orchard value balances.
    pub fn balances(&self) -> (Option<i64>, Option<i64>) {
        self.value_balances
    }

    /// Returns compact transparent tx data.
    pub fn transparent(&self) -> &TransparentCompactTx {
        &self.transparent
    }

    /// Returns compact sapling tx data.
    pub fn sapling(&self) -> &SaplingCompactTx {
        &self.sapling
    }

    /// Returns compact orchard tx data.
    pub fn orchard(&self) -> &[CompactOrchardAction] {
        &self.orchard
    }

    /// Converts this `TxData` into a `CompactTx` protobuf message with an optional fee.
    pub fn to_compact_tx(
        &self,
        fee: Option<u32>,
    ) -> zaino_proto::proto::compact_formats::CompactTx {
        let fee = fee.unwrap_or(0);

        let spends = self
            .sapling()
            .spends()
            .iter()
            .map(
                |s| zaino_proto::proto::compact_formats::CompactSaplingSpend {
                    nf: s.nullifier().to_vec(),
                },
            )
            .collect();

        let outputs = self
            .sapling()
            .outputs()
            .iter()
            .map(
                |o| zaino_proto::proto::compact_formats::CompactSaplingOutput {
                    cmu: o.cmu().to_vec(),
                    ephemeral_key: o.ephemeral_key().to_vec(),
                    ciphertext: o.ciphertext().to_vec(),
                },
            )
            .collect();

        let actions = self
            .orchard()
            .iter()
            .map(
                |a| zaino_proto::proto::compact_formats::CompactOrchardAction {
                    nullifier: a.nullifier().to_vec(),
                    cmx: a.cmx().to_vec(),
                    ephemeral_key: a.ephemeral_key().to_vec(),
                    ciphertext: a.ciphertext().to_vec(),
                },
            )
            .collect();

        zaino_proto::proto::compact_formats::CompactTx {
            index: self.index(),
            hash: self.txid().to_vec(),
            fee,
            spends,
            outputs,
            actions,
        }
    }
}

/// TryFrom inputs:
/// - Transaction Index
/// - Full Transaction
impl TryFrom<(u64, zaino_fetch::chain::transaction::FullTransaction)> for TxData {
    type Error = String;

    fn try_from(
        (index, tx): (u64, zaino_fetch::chain::transaction::FullTransaction),
    ) -> Result<Self, Self::Error> {
        let txid_vec = tx.tx_id();
        let txid: [u8; 32] = txid_vec
            .try_into()
            .map_err(|_| "txid must be 32 bytes".to_string())?;

        let value_balances = tx.value_balances();

        let vin: Vec<TxInCompact> = tx
            .transparent_inputs()
            .into_iter()
            .map(|(prev_txid, prev_index, _)| {
                let prev_txid_arr: [u8; 32] = prev_txid
                    .try_into()
                    .map_err(|_| "prev_txid must be 32 bytes".to_string())?;
                Ok::<_, String>(TxInCompact::new(prev_txid_arr, prev_index))
            })
            .collect::<Result<_, _>>()?;

        let vout: Vec<TxOutCompact> = tx
            .transparent_outputs()
            .into_iter()
            .filter_map(|(value, script_hash)| {
                if script_hash.len() == 21 {
                    let script_type = script_hash[0];
                    let mut hash_bytes = [0u8; 20];
                    hash_bytes.copy_from_slice(&script_hash[1..]);
                    TxOutCompact::new(value, hash_bytes, script_type)
                } else {
                    let mut fallback = [0u8; 20];
                    let usable_len = script_hash.len().min(20);
                    fallback[..usable_len].copy_from_slice(&script_hash[..usable_len]);
                    Some(TxOutCompact::new(
                        value,
                        fallback,
                        ScriptType::NonStandard as u8,
                    )?)
                }
            })
            .collect();

        let transparent = TransparentCompactTx::new(vin, vout);

        let spends: Vec<CompactSaplingSpend> = tx
            .shielded_spends()
            .into_iter()
            .map(|nf| {
                let arr: [u8; 32] = nf
                    .try_into()
                    .map_err(|_| "sapling nullifier must be 32 bytes".to_string())?;
                Ok::<_, String>(CompactSaplingSpend::new(arr))
            })
            .collect::<Result<_, _>>()?;

        let outputs: Vec<CompactSaplingOutput> = tx
            .shielded_outputs()
            .into_iter()
            .map(|(cmu, epk, ct)| {
                let cmu: [u8; 32] = cmu
                    .try_into()
                    .map_err(|_| "cmu must be 32 bytes".to_string())?;
                let epk: [u8; 32] = epk
                    .try_into()
                    .map_err(|_| "ephemeral_key must be 32 bytes".to_string())?;
                let ct: [u8; 52] = ct
                    .get(..52)
                    .ok_or("ciphertext must be at least 52 bytes")?
                    .try_into()
                    .map_err(|_| "ciphertext must be 52 bytes".to_string())?;
                Ok::<_, String>(CompactSaplingOutput::new(cmu, epk, ct))
            })
            .collect::<Result<_, _>>()?;

        let sapling = SaplingCompactTx::new(spends, outputs);

        let orchard = tx
            .orchard_actions()
            .into_iter()
            .map(|(nf, cmx, epk, ct)| {
                let nf: [u8; 32] = nf
                    .try_into()
                    .map_err(|_| "orchard nullifier must be 32 bytes".to_string())?;
                let cmx: [u8; 32] = cmx
                    .try_into()
                    .map_err(|_| "orchard cmx must be 32 bytes".to_string())?;
                let epk: [u8; 32] = epk
                    .try_into()
                    .map_err(|_| "orchard ephemeral_key must be 32 bytes".to_string())?;
                let ct: [u8; 52] = ct
                    .get(..52)
                    .ok_or("orchard ciphertext must be at least 52 bytes")?
                    .try_into()
                    .map_err(|_| "orchard ciphertext must be 52 bytes".to_string())?;
                Ok::<_, String>(CompactOrchardAction::new(nf, cmx, epk, ct))
            })
            .collect::<Result<_, _>>()?;

        Ok(TxData::new(
            index,
            txid,
            value_balances,
            transparent,
            sapling,
            orchard,
        ))
    }
}

impl CBORTagged for TxData {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::TxData.tag()]
    }
}

impl CBORTaggedEncodable for TxData {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(self.index),
            CBOR::from(&self.txid[..]),
            match self.value_balances.0 {
                Some(v) => CBOR::from(v),
                None => CBOR::null(),
            },
            match self.value_balances.1 {
                Some(v) => CBOR::from(v),
                None => CBOR::null(),
            },
            self.transparent.tagged_cbor(),
            self.sapling.tagged_cbor(),
            CBOR::from(
                self.orchard
                    .iter()
                    .map(|a| a.tagged_cbor())
                    .collect::<Vec<_>>(),
            ),
        ])
    }
}

impl CBORTaggedDecodable for TxData {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 7 => {
                let index: u64 = arr[0].clone().try_into()?;
                let txid_vec: Vec<u8> = arr[1].clone().try_into()?;
                let txid: [u8; 32] = txid_vec.try_into().map_err(|_| dcbor::Error::WrongType)?;

                let sapling_value = if arr[2].is_null() {
                    None
                } else {
                    Some(arr[2].clone().try_into()?)
                };
                let orchard_value = if arr[3].is_null() {
                    None
                } else {
                    Some(arr[3].clone().try_into()?)
                };

                let transparent = TransparentCompactTx::try_from(arr[4].clone())?;
                let sapling = SaplingCompactTx::try_from(arr[5].clone())?;

                let orchard_arr = match arr[6].clone().into_case() {
                    CBORCase::Array(a) => a,
                    _ => return Err(dcbor::Error::WrongType),
                };

                let orchard = orchard_arr
                    .into_iter()
                    .map(CompactOrchardAction::try_from)
                    .collect::<Result<_, _>>()?;

                Ok(Self {
                    index,
                    txid,
                    value_balances: (sapling_value, orchard_value),
                    transparent,
                    sapling,
                    orchard,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for TxData {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact transaction inputs and outputs for transparent (unshielded) transactions.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TransparentCompactTx {
    /// Transaction inputs (spent outputs from previous transactions).
    vin: Vec<TxInCompact>,
    /// Transaction outputs (newly created UTXOs).
    vout: Vec<TxOutCompact>,
}

impl TransparentCompactTx {
    /// Creates a new TransparentCompactTx instance.
    pub fn new(vin: Vec<TxInCompact>, vout: Vec<TxOutCompact>) -> Self {
        Self { vin, vout }
    }

    /// Returns transparent inputs.
    pub fn inputs(&self) -> &[TxInCompact] {
        &self.vin
    }

    /// Returns transparent outputs.
    pub fn outputs(&self) -> &[TxOutCompact] {
        &self.vout
    }
}

impl CBORTagged for TransparentCompactTx {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::TransparentCompactTx.tag()]
    }
}

impl CBORTaggedEncodable for TransparentCompactTx {
    fn untagged_cbor(&self) -> CBOR {
        let vin_cbor = self.vin.iter().map(|i| i.tagged_cbor()).collect::<Vec<_>>();
        let vout_cbor = self
            .vout
            .iter()
            .map(|o| o.tagged_cbor())
            .collect::<Vec<_>>();
        CBOR::from(vec![CBOR::from(vin_cbor), CBOR::from(vout_cbor)])
    }
}

impl CBORTaggedDecodable for TransparentCompactTx {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let vin_arr = match arr[0].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };
                let vout_arr = match arr[1].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };

                let vin = vin_arr
                    .into_iter()
                    .map(TxInCompact::try_from)
                    .collect::<Result<_, _>>()?;

                let vout = vout_arr
                    .into_iter()
                    .map(TxOutCompact::try_from)
                    .collect::<Result<_, _>>()?;

                Ok(Self { vin, vout })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for TransparentCompactTx {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// A compact reference to a previously created transparent UTXO being spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxInCompact {
    /// Transaction ID of the output being spent.
    prevout_txid: [u8; 32],
    /// Index (position) of the output in the previous transaction being spent.
    prevout_index: u32,
}

impl TxInCompact {
    /// Creates a new TxInCompact instance.
    pub fn new(prevout_txid: [u8; 32], prevout_index: u32) -> Self {
        Self {
            prevout_txid,
            prevout_index,
        }
    }

    /// Returns txid of the transaction that holds the output being sent.
    pub fn prevout_txid(&self) -> &[u8; 32] {
        &self.prevout_txid
    }

    /// Returns the index of the output being sent within the transaction.
    pub fn prevout_index(&self) -> u32 {
        self.prevout_index
    }
}

impl CBORTagged for TxInCompact {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::TxInCompact.tag()]
    }
}

impl CBORTaggedEncodable for TxInCompact {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(&self.prevout_txid[..]),
            CBOR::from(self.prevout_index),
        ])
    }
}

impl CBORTaggedDecodable for TxInCompact {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let txid: Vec<u8> = arr[0].clone().try_into()?;
                let index: u32 = arr[1].clone().try_into()?;
                Ok(Self {
                    prevout_txid: txid.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    prevout_index: index,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for TxInCompact {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Identifies the type of transparent transaction output script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub enum ScriptType {
    /// Standard pay-to-public-key-hash (P2PKH) address (`t1...`).
    P2PKH = 0x00,
    /// Standard pay-to-script-hash (P2SH) address (`t3...`).
    P2SH = 0x01,
    /// Non-standard output script (rare).
    NonStandard = 0xFF,
}

impl TryFrom<u8> for ScriptType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(ScriptType::P2PKH),
            0x01 => Ok(ScriptType::P2SH),
            0xFF => Ok(ScriptType::NonStandard),
            _ => Err(()),
        }
    }
}

impl ScriptType {
    /// Returns ScriptType as a String.
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptType::P2PKH => "P2PKH",
            ScriptType::P2SH => "P2SH",
            ScriptType::NonStandard => "NonStandard",
        }
    }
}

impl CBORTagged for ScriptType {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::ScriptType.tag()]
    }
}

impl CBORTaggedEncodable for ScriptType {
    fn untagged_cbor(&self) -> CBOR {
        (*self as u8).into()
    }
}

impl CBORTaggedDecodable for ScriptType {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        let value: u8 = cbor.try_into()?;
        ScriptType::try_from(value).map_err(|_| dcbor::Error::WrongType)
    }
}

impl TryFrom<CBOR> for ScriptType {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact representation of a transparent output, optimized for indexing and efficient querying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxOutCompact {
    /// Amount of ZEC sent to this output (in zatoshis).
    value: u64,
    /// 20-byte hash representation of the script or address this output pays to.
    script_hash: [u8; 20],
    /// Type indicator for the output's script/address type, enabling efficient address reconstruction.
    script_type: u8,
}

impl TxOutCompact {
    /// Creates a new TxOutCompact instance.
    pub fn new(value: u64, script_hash: [u8; 20], script_type: u8) -> Option<Self> {
        if ScriptType::try_from(script_type).is_ok() {
            Some(Self {
                value,
                script_hash,
                script_type,
            })
        } else {
            None
        }
    }

    /// Returns the valuse in zatoshi sent in this output.
    pub fn value(&self) -> u64 {
        self.value
    }

    /// Returns script hash.
    pub fn script_hash(&self) -> &[u8; 20] {
        &self.script_hash
    }

    /// Returns script type u8.
    pub fn script_type(&self) -> u8 {
        self.script_type
    }

    /// Returns script type Enum.
    pub fn script_type_enum(&self) -> Option<ScriptType> {
        ScriptType::try_from(self.script_type).ok()
    }
}

impl CBORTagged for TxOutCompact {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::TxOutCompact.tag()]
    }
}

impl CBORTaggedEncodable for TxOutCompact {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(self.value),
            CBOR::from(&self.script_hash[..]),
            CBOR::from(self.script_type),
        ])
    }
}

impl CBORTaggedDecodable for TxOutCompact {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 3 => {
                let value = arr[0].clone().try_into()?;
                let hash: Vec<u8> = arr[1].clone().try_into()?;
                let stype: u8 = arr[2].clone().try_into()?;
                Ok(Self {
                    value,
                    script_hash: hash.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    script_type: stype,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for TxOutCompact {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact representation of Sapling shielded transaction data for wallet scanning.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct SaplingCompactTx {
    /// Shielded spends (notes being consumed).
    spend: Vec<CompactSaplingSpend>,
    /// Shielded outputs (new notes created).
    output: Vec<CompactSaplingOutput>,
}

impl SaplingCompactTx {
    /// Creates a new SaplingCompactTx instance.
    pub fn new(spend: Vec<CompactSaplingSpend>, output: Vec<CompactSaplingOutput>) -> Self {
        Self { spend, output }
    }

    /// Returns sapling spends.
    pub fn spends(&self) -> &[CompactSaplingSpend] {
        &self.spend
    }

    /// Returns sapling outputs
    pub fn outputs(&self) -> &[CompactSaplingOutput] {
        &self.output
    }
}

impl CBORTagged for SaplingCompactTx {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::SaplingCompactTx.tag()]
    }
}

impl CBORTaggedEncodable for SaplingCompactTx {
    fn untagged_cbor(&self) -> CBOR {
        let spend_cbor = self
            .spend
            .iter()
            .map(|s| s.tagged_cbor())
            .collect::<Vec<_>>();
        let output_cbor = self
            .output
            .iter()
            .map(|o| o.tagged_cbor())
            .collect::<Vec<_>>();
        CBOR::from(vec![CBOR::from(spend_cbor), CBOR::from(output_cbor)])
    }
}

impl CBORTaggedDecodable for SaplingCompactTx {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let spend_arr = match arr[0].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };
                let output_arr = match arr[1].clone().into_case() {
                    CBORCase::Array(inner) => inner,
                    _ => return Err(dcbor::Error::WrongType),
                };

                let spend = spend_arr
                    .into_iter()
                    .map(CompactSaplingSpend::try_from)
                    .collect::<Result<_, _>>()?;
                let output = output_arr
                    .into_iter()
                    .map(CompactSaplingOutput::try_from)
                    .collect::<Result<_, _>>()?;

                Ok(Self { spend, output })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for SaplingCompactTx {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact representation of a Sapling shielded spend (consuming a previous shielded note).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactSaplingSpend {
    /// Nullifier of the Sapling note being spent, prevents double spends.
    nf: [u8; 32],
}

impl CompactSaplingSpend {
    /// Creates a new CompactSaplingSpend instance.
    pub fn new(nf: [u8; 32]) -> Self {
        Self { nf }
    }

    /// Returns sapling nullifier.
    pub fn nullifier(&self) -> &[u8; 32] {
        &self.nf
    }
}

impl CBORTagged for CompactSaplingSpend {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::CompactSaplingSpend.tag()]
    }
}

impl CBORTaggedEncodable for CompactSaplingSpend {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(&self.nf[..])
    }
}

impl CBORTaggedDecodable for CompactSaplingSpend {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        let nf: Vec<u8> = cbor.try_into()?;
        Ok(Self {
            nf: nf.try_into().map_err(|_| dcbor::Error::WrongType)?,
        })
    }
}

impl TryFrom<CBOR> for CompactSaplingSpend {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact representation of a newly created Sapling shielded note output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactSaplingOutput {
    /// Commitment of the newly created shielded note.
    cmu: [u8; 32],
    /// Ephemeral public key used by receivers to detect/decrypt the note.
    ephemeral_key: [u8; 32],
    /// Encrypted note ciphertext (minimal required portion).
    #[cfg_attr(test, serde(with = "serde_arrays::fixed_52"))]
    ciphertext: [u8; 52],
}

impl CompactSaplingOutput {
    /// Creates a new CompactSaplingOutput instance.
    pub fn new(cmu: [u8; 32], ephemeral_key: [u8; 32], ciphertext: [u8; 52]) -> Self {
        Self {
            cmu,
            ephemeral_key,
            ciphertext,
        }
    }

    /// Returns cmu.
    pub fn cmu(&self) -> &[u8; 32] {
        &self.cmu
    }

    /// Returns ephemeral key.
    pub fn ephemeral_key(&self) -> &[u8; 32] {
        &self.ephemeral_key
    }

    /// Returns ciphertext.
    pub fn ciphertext(&self) -> &[u8; 52] {
        &self.ciphertext
    }
}

impl CBORTagged for CompactSaplingOutput {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::CompactSaplingOutput.tag()]
    }
}

impl CBORTaggedEncodable for CompactSaplingOutput {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(&self.cmu[..]),
            CBOR::from(&self.ephemeral_key[..]),
            CBOR::from(&self.ciphertext[..]),
        ])
    }
}

impl CBORTaggedDecodable for CompactSaplingOutput {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 3 => {
                let cmu: Vec<u8> = arr[0].clone().try_into()?;
                let epk: Vec<u8> = arr[1].clone().try_into()?;
                let ct: Vec<u8> = arr[2].clone().try_into()?;
                Ok(Self {
                    cmu: cmu.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    ephemeral_key: epk.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    ciphertext: ct.try_into().map_err(|_| dcbor::Error::WrongType)?,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for CompactSaplingOutput {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Compact representation of Orchard shielded action (note spend or output).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactOrchardAction {
    /// Nullifier preventing double spends of the Orchard note.
    nullifier: [u8; 32],
    /// Commitment of the new Orchard note created.
    cmx: [u8; 32],
    /// Ephemeral public key for detecting and decrypting Orchard notes.
    ephemeral_key: [u8; 32],
    /// Encrypted ciphertext of the Orchard note (minimal required portion).
    #[cfg_attr(test, serde(with = "serde_arrays::fixed_52"))]
    ciphertext: [u8; 52],
}

impl CompactOrchardAction {
    /// Creates a new CompactOrchardAction instance.
    pub fn new(
        nullifier: [u8; 32],
        cmx: [u8; 32],
        ephemeral_key: [u8; 32],
        ciphertext: [u8; 52],
    ) -> Self {
        Self {
            nullifier,
            cmx,
            ephemeral_key,
            ciphertext,
        }
    }

    /// Returns orchard nullifier.
    pub fn nullifier(&self) -> &[u8; 32] {
        &self.nullifier
    }

    /// Returns cmx.
    pub fn cmx(&self) -> &[u8; 32] {
        &self.cmx
    }

    /// Returns ephemeral key.
    pub fn ephemeral_key(&self) -> &[u8; 32] {
        &self.ephemeral_key
    }

    /// Returns ciphertext.
    pub fn ciphertext(&self) -> &[u8; 52] {
        &self.ciphertext
    }
}

impl CBORTagged for CompactOrchardAction {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::CompactOrchardAction.tag()]
    }
}

impl CBORTaggedEncodable for CompactOrchardAction {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(&self.nullifier[..]),
            CBOR::from(&self.cmx[..]),
            CBOR::from(&self.ephemeral_key[..]),
            CBOR::from(&self.ciphertext[..]),
        ])
    }
}

impl CBORTaggedDecodable for CompactOrchardAction {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 4 => {
                let nullifier: Vec<u8> = arr[0].clone().try_into()?;
                let cmx: Vec<u8> = arr[1].clone().try_into()?;
                let ephemeral_key: Vec<u8> = arr[2].clone().try_into()?;
                let ciphertext: Vec<u8> = arr[3].clone().try_into()?;
                Ok(Self {
                    nullifier: nullifier.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    cmx: cmx.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    ephemeral_key: ephemeral_key
                        .try_into()
                        .map_err(|_| dcbor::Error::WrongType)?,
                    ciphertext: ciphertext.try_into().map_err(|_| dcbor::Error::WrongType)?,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for CompactOrchardAction {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Represents explicitly recorded spent transparent outputs within a block, facilitating fast indexing and rollbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct SpentOutpoint {
    /// Transaction ID of the output that was spent.
    txid: [u8; 32],
    /// The index of the output within the previous transaction being spent.
    vout: u32,
}

impl SpentOutpoint {
    /// Creates a new SpentOutpoint instance.
    pub fn new(txid: [u8; 32], vout: u32) -> Self {
        Self { txid, vout }
    }

    /// Returns the Txid of the transaction the outpoint was spent in.
    pub fn txid(&self) -> &[u8; 32] {
        &self.txid
    }

    /// Index of the output within the transaction.
    pub fn vout(&self) -> u32 {
        self.vout
    }
}

impl CBORTagged for SpentOutpoint {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::SpentOutpoint.tag()]
    }
}

impl CBORTaggedEncodable for SpentOutpoint {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![CBOR::from(&self.txid[..]), CBOR::from(self.vout)])
    }
}

impl CBORTaggedDecodable for SpentOutpoint {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 2 => {
                let txid: Vec<u8> = arr[0].clone().try_into()?;
                let vout: u32 = arr[1].clone().try_into()?;
                Ok(Self {
                    txid: txid.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    vout,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for SpentOutpoint {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

/// Root commitment for a state shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct ShardRoot {
    /// Shard commitment tree root (256-bit digest)
    hash: [u8; 32],
    /// Hash of the final block in this shard
    final_block_hash: [u8; 32],
    /// Height of the final block in this shard
    final_block_height: u32,
}

impl ShardRoot {
    /// Creates a new ShardRoot instance.
    pub fn new(hash: [u8; 32], final_block_hash: [u8; 32], final_block_height: u32) -> Self {
        Self {
            hash,
            final_block_hash,
            final_block_height,
        }
    }

    /// Returns commitment tree root.
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }

    /// Returns the hash of the final block in this shard.
    pub fn final_block_hash(&self) -> &[u8; 32] {
        &self.final_block_hash
    }

    /// Returns the Height of the final block in this shard.
    pub fn final_block_height(&self) -> u32 {
        self.final_block_height
    }
}

impl CBORTagged for ShardRoot {
    fn cbor_tags() -> Vec<Tag> {
        vec![CborTag::ShardRoot.tag()]
    }
}

impl CBORTaggedEncodable for ShardRoot {
    fn untagged_cbor(&self) -> CBOR {
        CBOR::from(vec![
            CBOR::from(&self.hash[..]),
            CBOR::from(&self.final_block_hash[..]),
            CBOR::from(self.final_block_height),
        ])
    }
}

impl CBORTaggedDecodable for ShardRoot {
    fn from_untagged_cbor(cbor: CBOR) -> dcbor::Result<Self> {
        match cbor.into_case() {
            CBORCase::Array(arr) if arr.len() == 3 => {
                let hash: Vec<u8> = arr[0].clone().try_into()?;
                let final_block_hash: Vec<u8> = arr[1].clone().try_into()?;
                let final_block_height: u32 = arr[2].clone().try_into()?;
                Ok(Self {
                    hash: hash.try_into().map_err(|_| dcbor::Error::WrongType)?,
                    final_block_hash: final_block_hash
                        .try_into()
                        .map_err(|_| dcbor::Error::WrongType)?,
                    final_block_height,
                })
            }
            _ => Err(dcbor::Error::WrongType),
        }
    }
}

impl TryFrom<CBOR> for ShardRoot {
    type Error = dcbor::Error;
    fn try_from(cbor: CBOR) -> dcbor::Result<Self> {
        Self::from_tagged_cbor(cbor)
    }
}

#[cfg(test)]
pub mod serde_arrays {
    use serde::{Deserialize, Deserializer, Serializer};

    pub mod fixed_52 {
        use super::*;
        pub fn serialize<S>(val: &[u8; 52], s: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            s.serialize_bytes(val)
        }

        pub fn deserialize<'de, D>(d: D) -> Result<[u8; 52], D::Error>
        where
            D: Deserializer<'de>,
        {
            let v: &[u8] = Deserialize::deserialize(d)?;
            v.try_into()
                .map_err(|_| serde::de::Error::custom("invalid length for [u8; 52]"))
        }
    }
}
