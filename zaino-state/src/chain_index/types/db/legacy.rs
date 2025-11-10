//! Type definitions for the chain index.
//!
//! MODULE RULES: These rules must **always** be followed with no exeptions.
//! - structs in this module must never use external types as fields directly,
//!   instead fundamental data should be saved into the struct, and from / into
//!   (or appropriate getters / setters) should be implemented.
//!
//! - structs in this module must implement ZainoVersionedSerialize and abide by
//!   the stringent version rules outlined in that trait.
//!
//! - structs in this module must never be changed without implementing a new version
//!   and implementing the necessary ZainoDB updates and migrations.
//!
//! This module is currently in transition from a large monolithic file to well-organized
//! submodules. The organized types have been moved to focused modules:
//!
//! ## Organized Modules
//! - [`super::primitives`] - Foundational types (hashes, heights, tree sizes, etc.)
//! - [`super::commitment`] - Commitment tree data structures and utilities
//!
//! ## Planned Module Organization
//! The remaining types in this file will be migrated to:
//! - `block.rs` - Block-related structures (BlockIndex, BlockData, IndexedBlock)
//! - `transaction.rs` - Transaction types (CompactTxData, TransparentCompactTx, etc.)
//! - `address.rs` - Address and UTXO types (AddrScript, Outpoint, etc.)
//! - `shielded.rs` - Shielded pool types (SaplingCompactTx, OrchardCompactTx, etc.)

// =============================================================================
// IMPORTS
// =============================================================================

use core2::io::{self, Read, Write};
use hex::{FromHex, ToHex};
use primitive_types::U256;
use std::{fmt, io::Cursor};

use crate::chain_index::encoding::{
    read_fixed_le, read_i64_le, read_option, read_u16_be, read_u32_be, read_u32_le, read_u64_le,
    read_vec, version, write_fixed_le, write_i64_le, write_option, write_u16_be, write_u32_be,
    write_u32_le, write_u64_le, write_vec, FixedEncodedLen, ZainoVersionedSerde,
};

use super::commitment::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};

// =============================================================================
// LEGACY TYPES AWAITING MIGRATION
// =============================================================================
// The types below should be extracted to their appropriate modules.
// Each section should be migrated as a complete unit to maintain clean git history.

/// Block hash (SHA256d hash of the block header).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHash(pub [u8; 32]);

impl BlockHash {
    /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
    pub fn bytes_in_display_order(&self) -> [u8; 32] {
        let mut reversed_bytes = self.0;
        reversed_bytes.reverse();
        reversed_bytes
    }

    /// Convert bytes in big-endian byte-order into a [`self::BlockHash`].
    pub fn from_bytes_in_display_order(bytes_in_display_order: &[u8; 32]) -> BlockHash {
        let mut internal_byte_order = *bytes_in_display_order;
        internal_byte_order.reverse();

        BlockHash(internal_byte_order)
    }
}

impl fmt::Display for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BlockHash({})", self.encode_hex::<String>())
    }
}

impl ToHex for &BlockHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl ToHex for BlockHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl FromHex for BlockHash {
    type Error = <[u8; 32] as FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hash = <[u8; 32]>::from_hex(hex)?;

        Ok(Self::from_bytes_in_display_order(&hash))
    }
}

impl From<[u8; 32]> for BlockHash {
    fn from(bytes: [u8; 32]) -> Self {
        BlockHash(bytes)
    }
}

impl From<BlockHash> for [u8; 32] {
    fn from(hash: BlockHash) -> Self {
        hash.0
    }
}

impl From<BlockHash> for zebra_chain::block::Hash {
    fn from(hash: BlockHash) -> Self {
        zebra_chain::block::Hash(hash.0)
    }
}

impl From<zebra_chain::block::Hash> for BlockHash {
    fn from(hash: zebra_chain::block::Hash) -> Self {
        BlockHash(hash.0)
    }
}

impl From<BlockHash> for zcash_primitives::block::BlockHash {
    fn from(hash: BlockHash) -> Self {
        zcash_primitives::block::BlockHash(hash.0)
    }
}

impl From<zcash_primitives::block::BlockHash> for BlockHash {
    fn from(hash: zcash_primitives::block::BlockHash) -> Self {
        BlockHash(hash.0)
    }
}

impl ZainoVersionedSerde for BlockHash {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(BlockHash(bytes))
    }
}

/// Hash = 32-byte body.
impl FixedEncodedLen for BlockHash {
    /// 32 bytes, LE
    const ENCODED_LEN: usize = 32;
}

/// Transaction hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TransactionHash(pub [u8; 32]);

impl TransactionHash {
    /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
    pub fn bytes_in_display_order(&self) -> [u8; 32] {
        let mut reversed_bytes = self.0;
        reversed_bytes.reverse();
        reversed_bytes
    }

    /// Convert bytes in big-endian byte-order into a [`self::TransactionHash`].
    pub fn from_bytes_in_display_order(bytes_in_display_order: &[u8; 32]) -> TransactionHash {
        let mut internal_byte_order = *bytes_in_display_order;
        internal_byte_order.reverse();

        TransactionHash(internal_byte_order)
    }
}

impl fmt::Display for TransactionHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl ToHex for &TransactionHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl ToHex for TransactionHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl FromHex for TransactionHash {
    type Error = <[u8; 32] as FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hash = <[u8; 32]>::from_hex(hex)?;

        Ok(Self::from_bytes_in_display_order(&hash))
    }
}

impl From<[u8; 32]> for TransactionHash {
    fn from(bytes: [u8; 32]) -> Self {
        TransactionHash(bytes)
    }
}

impl From<TransactionHash> for [u8; 32] {
    fn from(hash: TransactionHash) -> Self {
        hash.0
    }
}

impl From<TransactionHash> for zebra_chain::transaction::Hash {
    fn from(hash: TransactionHash) -> Self {
        zebra_chain::transaction::Hash(hash.0)
    }
}

impl From<zebra_chain::transaction::Hash> for TransactionHash {
    fn from(hash: zebra_chain::transaction::Hash) -> Self {
        TransactionHash(hash.0)
    }
}

impl From<TransactionHash> for zcash_primitives::transaction::TxId {
    fn from(hash: TransactionHash) -> Self {
        zcash_primitives::transaction::TxId::from_bytes(hash.0)
    }
}

impl From<zcash_primitives::transaction::TxId> for TransactionHash {
    fn from(hash: zcash_primitives::transaction::TxId) -> Self {
        TransactionHash(hash.into())
    }
}

impl ZainoVersionedSerde for TransactionHash {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(TransactionHash(bytes))
    }
}

/// Hash = 32-byte body.
impl FixedEncodedLen for TransactionHash {
    /// 32 bytes, LE
    const ENCODED_LEN: usize = 32;
}

/// Block height.
///
/// NOTE: Encoded as 4-byte big-endian byte-string to ensure height ordering
/// for keys in Lexicographically sorted B-Tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Height(pub(crate) u32);

/// The first block
pub const GENESIS_HEIGHT: Height = Height(0);

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

impl std::ops::Add<u32> for Height {
    type Output = Self;

    fn add(self, rhs: u32) -> Self::Output {
        Height(self.0 + rhs)
    }
}

impl std::ops::Sub<u32> for Height {
    type Output = Self;

    fn sub(self, rhs: u32) -> Self::Output {
        Height(self.0 - rhs)
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

impl ZainoVersionedSerde for Height {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Height must sort lexicographically - write **big-endian**
        write_u32_be(w, self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Height::try_from(raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Height = 4-byte big-endian body.
impl FixedEncodedLen for Height {
    /// 4 bytes, BE
    const ENCODED_LEN: usize = 4;
}

/// Numerical index of subtree / shard roots.
///
/// NOTE: Encoded as 4-byte big-endian byte-string to ensure height ordering
/// for keys in Lexicographically sorted B-Tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct ShardIndex(pub u32);

impl ZainoVersionedSerde for ShardIndex {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Index must sort lexicographically - write **big-endian**
        write_u32_be(w, self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Ok(ShardIndex(raw))
    }
}

/// Index = 4-byte big-endian body.
impl FixedEncodedLen for ShardIndex {
    /// 4 bytes (BE u32)
    const ENCODED_LEN: usize = 4;
}

/// A 20-byte hash160 *plus* a 1-byte ScriptType tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct AddrScript {
    hash: [u8; 20],
    script_type: u8,
}

impl AddrScript {
    /// Create from raw 20-byte hash + type byte.
    pub fn new(hash: [u8; 20], script_type: u8) -> Self {
        Self { hash, script_type }
    }

    /// Borrow the 20-byte hash.
    pub fn hash(&self) -> &[u8; 20] {
        &self.hash
    }

    /// The raw type byte (0x00 = P2PKH, 0x01 = P2SH, 0xFF = NonStandard).
    pub fn script_type(&self) -> u8 {
        self.script_type
    }

    /// Serialize into exactly 21 bytes: [hash‖type].
    pub fn to_raw_bytes(&self) -> [u8; 21] {
        let mut b = [0u8; 21];
        b[..20].copy_from_slice(&self.hash);
        b[20] = self.script_type;
        b
    }

    /// Parse from exactly 21 raw bytes.
    pub fn from_raw_bytes(b: &[u8; 21]) -> Self {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&b[..20]);
        let script_type = b[20];
        Self { hash, script_type }
    }

    /// Try to extract an AddrScript (20-byte hash + type) from a full locking script.
    pub fn from_script(script: &[u8]) -> Option<Self> {
        parse_standard_script(script).map(|(hash, stype)| AddrScript::new(hash, stype as u8))
    }

    /// Rebuild the canonical P2PKH or P2SH scriptPubKey bytes for this AddrScript.
    pub fn to_script_pubkey(&self) -> Option<Vec<u8>> {
        let stype = ScriptType::try_from(self.script_type).ok()?;
        build_standard_script(self.hash, stype)
    }
}

impl fmt::Display for AddrScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl ToHex for &AddrScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.to_raw_bytes().encode_hex()
    }
    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.to_raw_bytes().encode_hex_upper()
    }
}
impl ToHex for AddrScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }
    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl FromHex for AddrScript {
    type Error = <[u8; 21] as FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let raw: [u8; 21] = <[u8; 21]>::from_hex(hex)?;
        Ok(AddrScript::from_raw_bytes(&raw))
    }
}

impl From<[u8; 21]> for AddrScript {
    fn from(raw: [u8; 21]) -> Self {
        AddrScript::from_raw_bytes(&raw)
    }
}

impl From<AddrScript> for [u8; 21] {
    fn from(a: AddrScript) -> Self {
        a.to_raw_bytes()
    }
}

impl ZainoVersionedSerde for AddrScript {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<20, _>(&mut *w, &self.hash)?;
        w.write_all(&[self.script_type])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let hash = read_fixed_le::<20, _>(&mut *r)?;
        let mut buf = [0u8; 1];
        r.read_exact(&mut buf)?;
        Ok(AddrScript {
            hash,
            script_type: buf[0],
        })
    }
}

/// AddrScript = 21 bytes of body data.
impl FixedEncodedLen for AddrScript {
    /// 20 bytes, LE + 1 byte script type
    const ENCODED_LEN: usize = 21;
}

/// Reference to a spent transparent UTXO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Outpoint {
    /// Transaction ID of the UTXO being spent.
    prev_txid: [u8; 32],
    /// Index of that output in the previous transaction.
    prev_index: u32,
}

impl Outpoint {
    /// Construct a new outpoint.
    pub fn new(prev_txid: [u8; 32], prev_index: u32) -> Self {
        Self {
            prev_txid,
            prev_index,
        }
    }

    /// Build from a *display-order* txid.
    pub fn new_from_be(txid_be: &[u8; 32], index: u32) -> Self {
        let le = TransactionHash::from_bytes_in_display_order(txid_be).0;
        Self::new(le, index)
    }

    /// Returns the txid of the transaction being spent.
    pub fn prev_txid(&self) -> &[u8; 32] {
        &self.prev_txid
    }

    /// Returns the outpoint index withing the transaction.
    pub fn prev_index(&self) -> u32 {
        self.prev_index
    }
}

impl ZainoVersionedSerde for Outpoint {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.prev_txid)?;
        write_u32_le(&mut w, self.prev_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let txid = read_fixed_le::<32, _>(&mut r)?;
        let index = read_u32_le(&mut r)?;
        Ok(Outpoint::new(txid, index))
    }
}

/// Outpoint = 32‐byte txid + 4-byte LE u32 index = 36 bytes
impl FixedEncodedLen for Outpoint {
    /// 32 byte txid + 4 byte tx index.
    const ENCODED_LEN: usize = 32 + 4;
}

// *** Block Level Objects ***

/// Metadata about the block used to identify and navigate the blockchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockIndex {
    /// The hash identifying this block uniquely.
    pub hash: BlockHash,
    /// The hash of this block's parent block (previous block in chain).
    pub parent_hash: BlockHash,
    /// The cumulative proof-of-work of the blockchain up to this block, used for chain selection.
    pub chainwork: ChainWork,
    /// The height of this block if it's in the current best chain. None if it's part of a fork.
    pub height: Option<Height>,
}

impl BlockIndex {
    /// Constructs a new `BlockIndex`.
    pub fn new(
        hash: BlockHash,
        parent_hash: BlockHash,
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
    pub fn hash(&self) -> &BlockHash {
        &self.hash
    }

    /// Returns the hash of the parent block.
    pub fn parent_hash(&self) -> &BlockHash {
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
}

impl ZainoVersionedSerde for BlockIndex {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        self.hash.serialize(&mut w)?;
        self.parent_hash.serialize(&mut w)?;
        self.chainwork.serialize(&mut w)?;

        write_option(&mut w, &self.height, |w, h| h.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let hash = BlockHash::deserialize(&mut r)?;
        let parent_hash = BlockHash::deserialize(&mut r)?;
        let chainwork = ChainWork::deserialize(&mut r)?;
        let height = read_option(&mut r, |r| Height::deserialize(r))?;

        Ok(BlockIndex::new(hash, parent_hash, chainwork, height))
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

impl ZainoVersionedSerde for ChainWork {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(ChainWork(bytes))
    }
}

/// 32 byte body.
impl FixedEncodedLen for ChainWork {
    /// 32 bytes, LE
    const ENCODED_LEN: usize = 32;
}

/// Essential block header fields required for chain validation and serving block header data.
///
/// NOTE: Optional fields may be added for:
/// - hashLightClientRoot (FlyClient proofs)
/// - hashAuthDataRoot (ZIP-244 witness commitments)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockData {
    /// Version number of the block format (protocol upgrades).
    pub version: u32,
    /// Unix timestamp of when the block was mined (seconds since epoch).
    pub time: i64,
    /// Merkle root hash of all transaction IDs in the block (used for quick tx inclusion proofs).
    pub merkle_root: [u8; 32],
    /// Digest representing the block-commitments Merkle root (commitment to note states).
    /// - < V4: `hashFinalSaplingRoot` - Sapling note commitment tree root.
    /// - => V4: `hashBlockCommitments` - digest over hashLightClientRoot and hashAuthDataRoot.``
    pub block_commitments: [u8; 32],
    /// Compact difficulty target used for proof-of-work and difficulty calculation.
    pub bits: u32,
    /// Equihash nonse.
    pub nonce: [u8; 32],
    /// Equihash solution
    pub solution: EquihashSolution,
}

impl BlockData {
    /// Creates a new  BlockData instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        time: i64,
        merkle_root: [u8; 32],
        block_commitments: [u8; 32],
        bits: u32,
        nonse: [u8; 32],
        solution: EquihashSolution,
    ) -> Self {
        Self {
            version,
            time,
            merkle_root,
            block_commitments,
            bits,
            nonce: nonse,
            solution,
        }
    }

    /// Convert zebra block commitment to 32-byte array
    pub fn commitment_to_bytes(commitment: zebra_chain::block::Commitment) -> [u8; 32] {
        match commitment {
            zebra_chain::block::Commitment::PreSaplingReserved(bytes) => bytes,
            zebra_chain::block::Commitment::FinalSaplingRoot(root) => root.into(),
            zebra_chain::block::Commitment::ChainHistoryActivationReserved => [0; 32],
            zebra_chain::block::Commitment::ChainHistoryRoot(chain_history_mmr_root_hash) => {
                chain_history_mmr_root_hash.bytes_in_serialized_order()
            }
            zebra_chain::block::Commitment::ChainHistoryBlockTxAuthCommitment(
                chain_history_block_tx_auth_commitment_hash,
            ) => chain_history_block_tx_auth_commitment_hash.bytes_in_serialized_order(),
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

    /// Returns block finalSaplingRoot or authDataRoot depending on version.
    pub fn block_commitments(&self) -> &[u8; 32] {
        &self.block_commitments
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

    /// Returns Equihash Nonse.
    pub fn nonse(&self) -> [u8; 32] {
        self.nonce
    }

    /// Returns Equihash Nonse.
    pub fn solution(&self) -> EquihashSolution {
        self.solution
    }
}

impl ZainoVersionedSerde for BlockData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w; // re-borrow

        write_u32_le(&mut w, self.version)?;
        write_i64_le(&mut w, self.time)?;

        write_fixed_le::<32, _>(&mut w, &self.merkle_root)?;
        write_fixed_le::<32, _>(&mut w, &self.block_commitments)?;

        write_u32_le(&mut w, self.bits)?;
        write_fixed_le::<32, _>(&mut w, &self.nonce)?;

        self.solution.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let version = read_u32_le(&mut r)?;
        let time = read_i64_le(&mut r)?;

        let merkle_root = read_fixed_le::<32, _>(&mut r)?;
        let block_commitments = read_fixed_le::<32, _>(&mut r)?;

        let bits = read_u32_le(&mut r)?;
        let nonse = read_fixed_le::<32, _>(&mut r)?;

        let solution = EquihashSolution::deserialize(&mut r)?;

        Ok(BlockData::new(
            version,
            time,
            merkle_root,
            block_commitments,
            bits,
            nonse,
            solution,
        ))
    }
}

/// Equihash solution as it appears in a block header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
// NOTE: if memory usage becomes too large we could move this data to the heap.
#[allow(clippy::large_enum_variant)]
pub enum EquihashSolution {
    /// 200-9 solution (mainnet / testnet).
    #[cfg_attr(test, serde(with = "serde_arrays"))]
    Standard([u8; 1344]),
    /// 48-5 solution (regtest).
    #[cfg_attr(test, serde(with = "serde_arrays"))]
    Regtest([u8; 36]),
}

impl From<zebra_chain::work::equihash::Solution> for EquihashSolution {
    fn from(value: zebra_chain::work::equihash::Solution) -> Self {
        match value {
            zebra_chain::work::equihash::Solution::Common(array) => Self::Standard(array),
            zebra_chain::work::equihash::Solution::Regtest(array) => Self::Regtest(array),
        }
    }
}

impl EquihashSolution {
    /// Return a slice view (convenience).
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Standard(b) => b,
            Self::Regtest(b) => b,
        }
    }
}

impl TryFrom<Vec<u8>> for EquihashSolution {
    type Error = &'static str;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::try_from(bytes.as_slice())
    }
}

impl<'a> TryFrom<&'a [u8]> for EquihashSolution {
    type Error = &'static str;

    fn try_from(bytes: &'a [u8]) -> Result<Self, Self::Error> {
        match bytes.len() {
            1344 => {
                let mut arr = [0u8; 1344];
                arr.copy_from_slice(bytes);
                Ok(EquihashSolution::Standard(arr))
            }
            36 => {
                let mut arr = [0u8; 36];
                arr.copy_from_slice(bytes);
                Ok(EquihashSolution::Regtest(arr))
            }
            _ => Err("invalid Equihash solution length (expected 36 or 1344 bytes)"),
        }
    }
}

impl ZainoVersionedSerde for EquihashSolution {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        match self {
            Self::Standard(bytes) => {
                w.write_all(&[0])?;
                write_fixed_le::<1344, _>(&mut w, bytes)
            }
            Self::Regtest(bytes) => {
                w.write_all(&[1])?;
                write_fixed_le::<36, _>(&mut w, bytes)
            }
        }
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        match tag[0] {
            0 => {
                let bytes = read_fixed_le::<1344, _>(&mut r)?;
                Ok(EquihashSolution::Standard(bytes))
            }
            1 => {
                let bytes = read_fixed_le::<36, _>(&mut r)?;
                Ok(EquihashSolution::Regtest(bytes))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown Equihash variant tag {other}"),
            )),
        }
    }
}

/// Represents the indexing data of a single compact Zcash block used internally by Zaino.
/// Provides efficient indexing for blockchain state queries and updates.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct IndexedBlock {
    /// Metadata and indexing information for this block.
    pub index: BlockIndex,
    /// Essential header and metadata information for the block.
    pub data: BlockData,
    /// Compact representations of transactions in this block.
    pub transactions: Vec<CompactTxData>,
    /// Sapling and orchard commitment tree data for the chain
    /// *after this block has been applied.
    pub commitment_tree_data: CommitmentTreeData,
}

impl IndexedBlock {
    /// Creates a new `IndexedBlock`.
    pub fn new(
        index: BlockIndex,
        data: BlockData,
        tx: Vec<CompactTxData>,
        commitment_tree_data: CommitmentTreeData,
    ) -> Self {
        Self {
            index,
            data,
            transactions: tx,
            commitment_tree_data,
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
    pub fn transactions(&self) -> &[CompactTxData] {
        &self.transactions
    }

    /// Returns the commitment tree data for this block.
    pub fn commitment_tree_data(&self) -> &CommitmentTreeData {
        &self.commitment_tree_data
    }

    /// Returns the block hash.
    pub fn hash(&self) -> &BlockHash {
        self.index.hash()
    }

    /// Returns the block height if available.
    pub fn height(&self) -> Option<Height> {
        self.index.height()
    }

    /// Returns the cumulative chainwork.
    pub fn chainwork(&self) -> &ChainWork {
        self.index.chainwork()
    }

    /// Returns the raw work value (targeted work contribution).
    pub fn work(&self) -> U256 {
        self.data.work()
    }

    /// Converts this `IndexedBlock` into a CompactBlock protobuf message using proto v4 format.
    pub fn to_compact_block(&self) -> zaino_proto::proto::compact_formats::CompactBlock {
        // NOTE: Returns u64::MAX if the block is not in the best chain.
        let height: u64 = self.height().map(|h| h.0.into()).unwrap_or(u64::MAX);

        let hash = self.hash().0.to_vec();
        let prev_hash = self.index().parent_hash().0.to_vec();

        let vtx: Vec<zaino_proto::proto::compact_formats::CompactTx> = self
            .transactions()
            .iter()
            .filter_map(|tx| {
                let has_shielded = !tx.sapling().spends().is_empty()
                    || !tx.sapling().outputs().is_empty()
                    || !tx.orchard().actions().is_empty();

                if !has_shielded {
                    return None;
                }

                Some(tx.to_compact_tx(None))
            })
            .collect();

        let sapling_commitment_tree_size = self.commitment_tree_data().sizes().sapling();
        let orchard_commitment_tree_size = self.commitment_tree_data().sizes().orchard();

        zaino_proto::proto::compact_formats::CompactBlock {
            proto_version: 4,
            height,
            hash,
            prev_hash,
            time: self.data().time() as u32,
            header: vec![],
            vtx,
            chain_metadata: Some(zaino_proto::proto::compact_formats::ChainMetadata {
                sapling_commitment_tree_size,
                orchard_commitment_tree_size,
            }),
        }
    }
}

impl ZainoVersionedSerde for IndexedBlock {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, mut w: &mut W) -> io::Result<()> {
        self.index.serialize(&mut w)?;
        self.data.serialize(&mut w)?;
        write_vec(&mut w, &self.transactions, |w, tx| tx.serialize(w))?;
        self.commitment_tree_data.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let index = BlockIndex::deserialize(&mut r)?;
        let data = BlockData::deserialize(&mut r)?;
        let tx = read_vec(&mut r, |r| CompactTxData::deserialize(r))?;
        let ctd = CommitmentTreeData::deserialize(&mut r)?;

        Ok(IndexedBlock::new(index, data, tx, ctd))
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
    )> for IndexedBlock
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

        let merkle_root: [u8; 32] = header
            .hash_merkle_root()
            .try_into()
            .map_err(|v: Vec<u8>| format!("merkle root must be 32 bytes, got {}", v.len()))?;

        let block_commitments: [u8; 32] = header
            .final_sapling_root()
            .try_into()
            .map_err(|v: Vec<u8>| format!("block commitment must be 32 bytes, got {}", v.len()))?;

        let n_bits_bytes = header.n_bits_bytes();
        if n_bits_bytes.len() != 4 {
            return Err("nBits must be 4 bytes".to_string());
        }
        let bits = u32::from_le_bytes(n_bits_bytes.try_into().unwrap());

        let nonse: [u8; 32] = header
            .nonce()
            .try_into()
            .map_err(|v: Vec<u8>| format!("nonse must be 32 bytes, got {}", v.len()))?;

        let solution = EquihashSolution::try_from(header.solution()).map_err(|_| {
            format!(
                "solution must be 32 or 1344 bytes, got {}",
                header.solution().len()
            )
        })?;

        // --- Convert transactions ---
        let mut sapling_note_count = 0;
        let mut orchard_note_count = 0;

        let full_transactions = full_block.transactions();
        let mut tx = Vec::with_capacity(full_transactions.len());

        for (i, ftx) in full_transactions.into_iter().enumerate() {
            let txdata = CompactTxData::try_from((i as u64, ftx))
                .map_err(|e| format!("TxData conversion failed at index {i}: {e}"))?;

            sapling_note_count += txdata.sapling().outputs().len();
            orchard_note_count += txdata.orchard().actions().len();

            tx.push(txdata);
        }

        // --- Compute commitment trees ---
        let sapling_root = final_sapling_root;
        let orchard_root = final_orchard_root;

        let commitment_tree_data = CommitmentTreeData::new(
            CommitmentTreeRoots::new(sapling_root, orchard_root),
            CommitmentTreeSizes::new(
                parent_sapling_size + sapling_note_count as u32,
                parent_orchard_size + orchard_note_count as u32,
            ),
        );

        // --- Compute chainwork ---
        let block_data = BlockData::new(
            header.version() as u32,
            header.time() as i64,
            merkle_root,
            block_commitments,
            bits,
            nonse,
            solution,
        );

        let chainwork = parent_chainwork.add(&ChainWork::from(block_data.work()));

        // --- Final index and block data ---
        let index = BlockIndex::new(
            BlockHash::from(hash),
            BlockHash::from(parent_hash),
            chainwork,
            Some(height),
        );

        Ok(IndexedBlock::new(
            index,
            block_data,
            tx,
            commitment_tree_data,
        ))
    }
}

/// Tree root data from blockchain source
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactTxData {
    /// The index (position) of this transaction within its block (0-based).
    index: u64,
    /// Unique identifier (hash) of the transaction, used for lookup and indexing.
    txid: TransactionHash,
    /// Compact representation of transparent inputs/outputs in the transaction.
    transparent: TransparentCompactTx,
    /// Compact representation of Sapling shielded data.
    sapling: SaplingCompactTx,
    /// Compact representation of Orchard actions (shielded pool transactions).
    orchard: OrchardCompactTx,
}

impl CompactTxData {
    /// Creates a new TxData instance.
    pub fn new(
        index: u64,
        txid: TransactionHash,
        transparent: TransparentCompactTx,
        sapling: SaplingCompactTx,
        orchard: OrchardCompactTx,
    ) -> Self {
        Self {
            index,
            txid,
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
    pub fn txid(&self) -> &TransactionHash {
        &self.txid
    }

    /// Returns sapling and orchard value balances.
    pub fn balances(&self) -> (Option<i64>, Option<i64>) {
        (self.sapling.value, self.orchard.value)
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
    pub fn orchard(&self) -> &OrchardCompactTx {
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
            .actions()
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
            hash: self.txid().0.to_vec(),
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
impl TryFrom<(u64, zaino_fetch::chain::transaction::FullTransaction)> for CompactTxData {
    type Error = String;

    fn try_from(
        (index, tx): (u64, zaino_fetch::chain::transaction::FullTransaction),
    ) -> Result<Self, Self::Error> {
        let txid_vec = tx.tx_id();
        // NOTE: Is this byte order correct?
        let txid: [u8; 32] = txid_vec
            .try_into()
            .map_err(|_| "txid must be 32 bytes".to_string())?;

        let (sapling_balance, orchard_balance) = tx.value_balances();

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

        //TODO: We should error handle on these, a failure here should probably be
        // reacted to
        let vout: Vec<TxOutCompact> = tx
            .transparent_outputs()
            .into_iter()
            .filter_map(|(value, script)| {
                if let Some((hash20, stype)) = parse_standard_script(&script) {
                    TxOutCompact::new(value, hash20, stype as u8)
                } else {
                    let mut fallback = [0u8; 20];
                    let copy_len = script.len().min(20);
                    fallback[..copy_len].copy_from_slice(&script[..copy_len]);
                    TxOutCompact::new(value, fallback, ScriptType::NonStandard as u8)
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

        let sapling = SaplingCompactTx::new(sapling_balance, spends, outputs);

        let actions: Vec<CompactOrchardAction> = tx
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

        let orchard = OrchardCompactTx::new(orchard_balance, actions);

        Ok(CompactTxData::new(
            index,
            // NOTE: do we need to use from_bytes_in_display_order here?
            txid.into(),
            transparent,
            sapling,
            orchard,
        ))
    }
}

impl ZainoVersionedSerde for CompactTxData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, mut w: &mut W) -> io::Result<()> {
        write_u64_le(&mut w, self.index)?;

        self.txid.serialize(&mut w)?;
        self.transparent.serialize(&mut w)?;
        self.sapling.serialize(&mut w)?;
        self.orchard.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let index = read_u64_le(&mut r)?;

        let txid = TransactionHash::deserialize(&mut r)?;
        let transparent = TransparentCompactTx::deserialize(&mut r)?;
        let sapling = SaplingCompactTx::deserialize(&mut r)?;
        let orchard = OrchardCompactTx::deserialize(&mut r)?;

        Ok(CompactTxData::new(
            index,
            txid,
            transparent,
            sapling,
            orchard,
        ))
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

impl ZainoVersionedSerde for TransparentCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_vec(&mut w, &self.vin, |w, txin| txin.serialize(w))?;
        write_vec(&mut w, &self.vout, |w, txout| txout.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let vin = read_vec(&mut r, |r| TxInCompact::deserialize(r))?;
        let vout = read_vec(&mut r, |r| TxOutCompact::deserialize(r))?;

        Ok(TransparentCompactTx::new(vin, vout))
    }
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

    /// Constructs a canonical "null prevout" (coinbase marker).
    pub fn null_prevout() -> Self {
        Self {
            prevout_txid: [0u8; 32],
            prevout_index: u32::MAX,
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

    /// `true` iff this input is the special “null” out-point used by a
    /// coinbase transaction (all-zero txid, index 0xffff_ffff).
    pub fn is_null_prevout(&self) -> bool {
        self.prevout_txid == [0u8; 32] && self.prevout_index == u32::MAX
    }
}

impl ZainoVersionedSerde for TxInCompact {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.prevout_txid)?;
        write_u32_le(&mut w, self.prevout_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let txid = read_fixed_le::<32, _>(&mut r)?;
        let idx = read_u32_le(&mut r)?;
        Ok(TxInCompact::new(txid, idx))
    }
}

/// TxInCompact = 36 bytes
impl FixedEncodedLen for TxInCompact {
    /// 32-byte txid + 4-byte LE index
    const ENCODED_LEN: usize = 32 + 4;
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

impl ZainoVersionedSerde for ScriptType {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&[*self as u8])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        ScriptType::try_from(b[0])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unknown ScriptType"))
    }
}

/// ScriptType = 1 byte
impl FixedEncodedLen for ScriptType {
    /// 1 byte
    const ENCODED_LEN: usize = 1;
}

/// Try to recognise a standard P2PKH / P2SH locking script.
/// Returns (payload-hash, ScriptType) on success.
pub(crate) fn parse_standard_script(script: &[u8]) -> Option<([u8; 20], ScriptType)> {
    // P2PKH 76 a9 14 <20-B hash> 88 ac
    const P2PKH_PREFIX: &[u8] = &[0x76, 0xa9, 0x14];
    const P2PKH_SUFFIX: &[u8] = &[0x88, 0xac];

    // P2SH  a9 14 <20-B hash> 87
    const P2SH_PREFIX: &[u8] = &[0xa9, 0x14];
    const P2SH_SUFFIX: &[u8] = &[0x87];

    if script.starts_with(P2PKH_PREFIX) && script.ends_with(P2PKH_SUFFIX) && script.len() == 25 {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&script[3..23]);
        return Some((hash, ScriptType::P2PKH));
    }
    if script.starts_with(P2SH_PREFIX) && script.ends_with(P2SH_SUFFIX) && script.len() == 23 {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&script[2..22]);
        return Some((hash, ScriptType::P2SH));
    }
    None
}

/// Reconstruct the canonical P2PKH or P2SH scriptPubKey for a 20-byte payload.
/// Returns `None` if given `ScriptType::NonStandard` (or any other unknown type).
pub(crate) fn build_standard_script(hash: [u8; 20], stype: ScriptType) -> Option<Vec<u8>> {
    const P2PKH_PREFIX: &[u8] = &[0x76, 0xa9, 0x14];
    const P2PKH_SUFFIX: &[u8] = &[0x88, 0xac];
    const P2PKH_LEN: usize = 25;

    const P2SH_PREFIX: &[u8] = &[0xa9, 0x14];
    const P2SH_SUFFIX: u8 = 0x87;
    const P2SH_LEN: usize = 23;

    match stype {
        ScriptType::P2PKH => {
            let mut script = Vec::with_capacity(P2PKH_LEN);
            script.extend_from_slice(P2PKH_PREFIX);
            script.extend_from_slice(&hash);
            script.extend_from_slice(P2PKH_SUFFIX);
            debug_assert!(script.len() == P2PKH_LEN);
            Some(script)
        }
        ScriptType::P2SH => {
            let mut script = Vec::with_capacity(P2SH_LEN);
            script.extend_from_slice(P2SH_PREFIX);
            script.extend_from_slice(&hash);
            script.push(P2SH_SUFFIX);
            debug_assert!(script.len() == P2SH_LEN);
            Some(script)
        }
        ScriptType::NonStandard => None,
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

impl<T: AsRef<[u8]>> TryFrom<(u64, T)> for TxOutCompact {
    type Error = ();

    fn try_from((value, script): (u64, T)) -> Result<Self, Self::Error> {
        let script_bytes = script.as_ref();

        if let Some(addr) = AddrScript::from_script(script_bytes) {
            TxOutCompact::new(value, *addr.hash(), addr.script_type()).ok_or(())
        } else if script_bytes.len() == 21 {
            let script_type = script_bytes[0];
            let mut hash_bytes = [0u8; 20];
            hash_bytes.copy_from_slice(&script_bytes[1..]);
            TxOutCompact::new(value, hash_bytes, script_type).ok_or(())
        } else {
            // fallback for nonstandard scripts
            let mut fallback = [0u8; 20];
            let usable_len = script_bytes.len().min(20);
            fallback[..usable_len].copy_from_slice(&script_bytes[..usable_len]);
            TxOutCompact::new(value, fallback, ScriptType::NonStandard as u8).ok_or(())
        }
    }
}

impl ZainoVersionedSerde for TxOutCompact {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u64_le(&mut w, self.value)?;
        write_fixed_le::<20, _>(&mut w, &self.script_hash)?;
        w.write_all(&[self.script_type])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let value = read_u64_le(&mut r)?;
        let script_hash = read_fixed_le::<20, _>(&mut r)?;

        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        TxOutCompact::new(value, script_hash, b[0])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid script_type"))
    }
}

/// TxOutCompact = 29 bytes
impl FixedEncodedLen for TxOutCompact {
    /// 8-byte LE value + 20-byte script hash + 1-byte type
    const ENCODED_LEN: usize = 8 + 20 + 1;
}

/// Compact representation of Sapling shielded transaction data for wallet scanning.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct SaplingCompactTx {
    /// Net Sapling value balance (before fees); `None` if no Sapling component.
    value: Option<i64>,
    /// Shielded spends (notes being consumed).
    spends: Vec<CompactSaplingSpend>,
    /// Shielded outputs (new notes created).
    outputs: Vec<CompactSaplingOutput>,
}

impl SaplingCompactTx {
    /// Creates a new SaplingCompactTx instance.
    pub fn new(
        value: Option<i64>,
        spends: Vec<CompactSaplingSpend>,
        outputs: Vec<CompactSaplingOutput>,
    ) -> Self {
        Self {
            value,
            spends,
            outputs,
        }
    }

    /// Returns the net sapling value balance (before fees); `None` if no sapling component.
    pub fn value(&self) -> Option<i64> {
        self.value
    }

    /// Returns sapling spends.
    pub fn spends(&self) -> &[CompactSaplingSpend] {
        &self.spends
    }

    /// Returns sapling outputs
    pub fn outputs(&self) -> &[CompactSaplingOutput] {
        &self.outputs
    }
}

impl ZainoVersionedSerde for SaplingCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_option(&mut w, &self.value, |w, v| write_i64_le(w, *v))?;
        write_vec(&mut w, &self.spends, |w, s| s.serialize(w))?;
        write_vec(&mut w, &self.outputs, |w, o| o.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let value = read_option(&mut r, |r| read_i64_le(r))?;
        let spends = read_vec(&mut r, |r| CompactSaplingSpend::deserialize(r))?;
        let outputs = read_vec(&mut r, |r| CompactSaplingOutput::deserialize(r))?;

        Ok(SaplingCompactTx::new(value, spends, outputs))
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

    /// Creates a Proto CompactSaplingSpend from this record.
    pub fn into_compact(&self) -> zaino_proto::proto::compact_formats::CompactSaplingSpend {
        zaino_proto::proto::compact_formats::CompactSaplingSpend {
            nf: self.nf.to_vec(),
        }
    }
}

impl ZainoVersionedSerde for CompactSaplingSpend {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.nf)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(CompactSaplingSpend::new(read_fixed_le::<32, _>(r)?))
    }
}

/// 32-byte nullifier
impl FixedEncodedLen for CompactSaplingSpend {
    /// 32 bytes
    const ENCODED_LEN: usize = 32;
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
    #[cfg_attr(test, serde(with = "serde_arrays"))]
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

    /// Creates a Proto CompactSaplingOutput from this record.
    pub fn into_compact(&self) -> zaino_proto::proto::compact_formats::CompactSaplingOutput {
        zaino_proto::proto::compact_formats::CompactSaplingOutput {
            cmu: self.cmu.to_vec(),
            ephemeral_key: self.ephemeral_key.to_vec(),
            ciphertext: self.ciphertext.to_vec(),
        }
    }
}

impl ZainoVersionedSerde for CompactSaplingOutput {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.cmu)?;
        write_fixed_le::<32, _>(&mut w, &self.ephemeral_key)?;
        write_fixed_le::<52, _>(&mut w, &self.ciphertext)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let cmu = read_fixed_le::<32, _>(&mut r)?;
        let epk = read_fixed_le::<32, _>(&mut r)?;
        let ciphertext = read_fixed_le::<52, _>(&mut r)?;
        Ok(CompactSaplingOutput::new(cmu, epk, ciphertext))
    }
}

/// 116 bytes
impl FixedEncodedLen for CompactSaplingOutput {
    /// 32-byte cmu + 32-byte ephemeral_key + 52-byte ciphertext
    const ENCODED_LEN: usize = 32 + 32 + 52;
}

/// Compact summary of all shielded activity in a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct OrchardCompactTx {
    /// Net Orchard value balance (before fees); `None` if no Orchard component.
    value: Option<i64>,
    /// Orchard actions (may be empty).
    actions: Vec<CompactOrchardAction>,
}

impl OrchardCompactTx {
    /// Creates a new CompactOrchardTx instance.
    pub fn new(value: Option<i64>, actions: Vec<CompactOrchardAction>) -> Self {
        Self { value, actions }
    }

    /// Returns the net orchard value balance (before fees); `None` if no Orchard component.
    pub fn value(&self) -> Option<i64> {
        self.value
    }

    /// Returns the orchard actions in this transaction.
    pub fn actions(&self) -> &[CompactOrchardAction] {
        &self.actions
    }
}

impl ZainoVersionedSerde for OrchardCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_option(&mut w, &self.value, |w, v| write_i64_le(w, *v))?;
        write_vec(&mut w, &self.actions, |w, a| a.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let value = read_option(&mut r, |r| read_i64_le(r))?;
        let actions = read_vec(&mut r, |r| CompactOrchardAction::deserialize(r))?;

        Ok(OrchardCompactTx::new(value, actions))
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
    #[cfg_attr(test, serde(with = "serde_arrays"))]
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

    /// Creates a Proto CompactOrchardAction from this record.
    pub fn into_compact(&self) -> zaino_proto::proto::compact_formats::CompactOrchardAction {
        zaino_proto::proto::compact_formats::CompactOrchardAction {
            nullifier: self.nullifier.to_vec(),
            cmx: self.cmx.to_vec(),
            ephemeral_key: self.ephemeral_key.to_vec(),
            ciphertext: self.ciphertext.to_vec(),
        }
    }
}

impl ZainoVersionedSerde for CompactOrchardAction {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.nullifier)?;
        write_fixed_le::<32, _>(&mut w, &self.cmx)?;
        write_fixed_le::<32, _>(&mut w, &self.ephemeral_key)?;
        write_fixed_le::<52, _>(&mut w, &self.ciphertext)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let nf = read_fixed_le::<32, _>(&mut r)?;
        let cmx = read_fixed_le::<32, _>(&mut r)?;
        let epk = read_fixed_le::<32, _>(&mut r)?;
        let ctxt = read_fixed_le::<52, _>(&mut r)?;
        Ok(CompactOrchardAction::new(nf, cmx, epk, ctxt))
    }
}

// CompactOrchardAction = 148 bytes
impl FixedEncodedLen for CompactOrchardAction {
    /// 32-byte nullifier + 32-byte cmx + 32-byte ephemeral_key + 52-byte ciphertext
    const ENCODED_LEN: usize = 32 + 32 + 32 + 52;
}

/// Identifies a transaction's location by block height and transaction index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxLocation {
    /// Block height in chain.
    block_height: u32,
    /// Transaction index in block.
    tx_index: u16,
}

impl TxLocation {
    /// Creates a new TxLocation instance.
    pub fn new(block_height: u32, tx_index: u16) -> Self {
        Self {
            block_height,
            tx_index,
        }
    }

    /// Returns the block height held in the TxLocation.
    pub fn block_height(&self) -> u32 {
        self.block_height
    }

    /// Returns the transaction index held in the TxLocation.
    pub fn tx_index(&self) -> u16 {
        self.tx_index
    }
}

impl ZainoVersionedSerde for TxLocation {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u32_be(&mut *w, self.block_height)?;
        write_u16_be(&mut *w, self.tx_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let block_height = read_u32_be(&mut *r)?;
        let tx_index = read_u16_be(&mut *r)?;
        Ok(TxLocation::new(block_height, tx_index))
    }
}

/// 6 bytes, BE encoded.
impl FixedEncodedLen for TxLocation {
    /// 4-byte big-endian block_index + 2-byte big-endian tx_index
    const ENCODED_LEN: usize = 4 + 2;
}

/// Single transparent-address activity record (input or output).
///
/// Note when flag is set to IS_INPUT, out_index is actually the index of the input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct AddrHistRecord {
    tx_location: TxLocation,
    out_index: u16,
    value: u64,
    flags: u8,
}

/* ----- flag helpers ----- */
impl AddrHistRecord {
    /// Flag mask for is_mined.
    pub const FLAG_MINED: u8 = 0b00000001;

    /// Flag mask for is_spent.
    pub const FLAG_SPENT: u8 = 0b00000010;

    /// Flag mask for is_input.
    pub const FLAG_IS_INPUT: u8 = 0b00000100;

    /// Creatues a new AddrHistRecord instance.
    pub fn new(tx_location: TxLocation, out_index: u16, value: u64, flags: u8) -> Self {
        Self {
            tx_location,
            out_index,
            value,
            flags,
        }
    }

    /// Returns the TxLocation in this record.
    pub fn tx_location(&self) -> TxLocation {
        self.tx_location
    }

    /// Returns the out index of this record.
    pub fn out_index(&self) -> u16 {
        self.out_index
    }

    /// Returns the value of this record.
    pub fn value(&self) -> u64 {
        self.value
    }

    /// Returns the flag byte of this record.
    pub fn flags(&self) -> u8 {
        self.flags
    }

    /// Returns true if this record is from a mined block.
    pub fn is_mined(&self) -> bool {
        self.flags & Self::FLAG_MINED != 0
    }

    /// Returns true if this record is a spend.
    pub fn is_spent(&self) -> bool {
        self.flags & Self::FLAG_SPENT != 0
    }

    /// Returns true if this record is an input.
    pub fn is_input(&self) -> bool {
        self.flags & Self::FLAG_IS_INPUT != 0
    }
}

impl ZainoVersionedSerde for AddrHistRecord {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.tx_location.serialize(&mut *w)?;
        write_u16_be(&mut *w, self.out_index)?;
        write_u64_le(&mut *w, self.value)?;
        w.write_all(&[self.flags])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let tx_location = TxLocation::deserialize(&mut *r)?;
        let out_index = read_u16_be(&mut *r)?;
        let value = read_u64_le(&mut *r)?;
        let mut flag = [0u8; 1];
        r.read_exact(&mut flag)?;

        Ok(AddrHistRecord::new(tx_location, out_index, value, flag[0]))
    }
}

/// 18 byte total
impl FixedEncodedLen for AddrHistRecord {
    ///  1 byte:  TxLocation tag
    /// +6 bytes: TxLocation body (4 BE block_index + 2 BE tx_index)
    /// +2 bytes: out_index (BE)
    /// +8 bytes: value     (LE)
    /// +1 byte : flags
    /// =18 bytes
    const ENCODED_LEN: usize = (TxLocation::ENCODED_LEN + 1) + 2 + 8 + 1;
}

/// AddrHistRecord database byte array.
///
/// Layout (all big-endian except `value`):
/// ```text
/// [0..4]   height
/// [4..6]   tx_index
/// [6..8]   vout
/// [8]      flags
/// [9..17]  value   (little-endian, matches Zcashd)
/// ```
///
/// Note when flag is set to IS_INPUT, vout is actually the index of the input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct AddrEventBytes([u8; 17]);

impl AddrEventBytes {
    const LEN: usize = 17;

    /// Flag mask for is_mined.
    pub const FLAG_MINED: u8 = 0x01;

    /// Flag mask for is_spent.
    pub const FLAG_SPENT: u8 = 0x02;

    /// Flag mask for is_input.
    pub const FLAG_IS_INPUT: u8 = 0x04;

    /// Create an [`AddrEventBytes`] from an [`AddrHistRecord`],
    /// returning an I/O error if any write fails.
    #[allow(dead_code)]
    pub(crate) fn from_record(rec: &AddrHistRecord) -> io::Result<Self> {
        let mut buf = [0u8; Self::LEN];
        let mut c = Cursor::new(&mut buf[..]);

        write_u32_be(&mut c, rec.tx_location.block_height)?;
        write_u16_be(&mut c, rec.tx_location.tx_index)?;
        write_u16_be(&mut c, rec.out_index)?;
        c.write_all(&[rec.flags])?;
        write_u64_le(&mut c, rec.value)?;

        Ok(AddrEventBytes(buf))
    }

    /// Create an [`AddrHistRecord`] from an [`AddrEventBytes`],
    /// returning an I/O error if any read fails or data is invalid.
    #[allow(dead_code)]
    pub(crate) fn as_record(&self) -> io::Result<AddrHistRecord> {
        let mut c = Cursor::new(&self.0[..]);

        let block_height = read_u32_be(&mut c)?;
        let tx_index = read_u16_be(&mut c)?;
        let out_index = read_u16_be(&mut c)?;
        let mut flag = [0u8; 1];
        c.read_exact(&mut flag)?;
        let value = read_u64_le(&mut c)?;

        Ok(AddrHistRecord::new(
            TxLocation::new(block_height, tx_index),
            out_index,
            value,
            flag[0],
        ))
    }
}

impl ZainoVersionedSerde for AddrEventBytes {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<17, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(AddrEventBytes(read_fixed_le::<17, _>(r)?))
    }
}

/// 17 byte body:
///
/// ```text
/// [0..4]   block_height (BE u32) | Block height
/// [4..6]   tx_index     (BE u16) | Transaction index within block
/// [6..8]   vout         (BE u16) | Input/output index within transaction
/// [8]      flags        ( u8 )   | Bitflags (mined/spent/input masks)
/// [9..17]  value        (LE u64) | Amount in zatoshi, little-endian
/// ```
impl FixedEncodedLen for AddrEventBytes {
    const ENCODED_LEN: usize = 17;
}

// *** Sharding ***

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

impl ZainoVersionedSerde for ShardRoot {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.hash)?;
        write_fixed_le::<32, _>(&mut w, &self.final_block_hash)?;
        write_u32_le(&mut w, self.final_block_height)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let hash = read_fixed_le::<32, _>(&mut r)?;
        let final_block_hash = read_fixed_le::<32, _>(&mut r)?;
        let final_block_height = read_u32_le(&mut r)?;
        Ok(ShardRoot::new(hash, final_block_hash, final_block_height))
    }
}

/// 68 byte body.
impl FixedEncodedLen for ShardRoot {
    /// 32 byte hash + 32 byte hash + 4 byte block height
    const ENCODED_LEN: usize = 32 + 32 + 4;
}

// *** Wrapper Objects ***

/// Holds full block header data,
/// split into chain indexeing data and additional header data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHeaderData {
    /// Chain indexing data
    index: BlockIndex,
    /// Block header data
    data: BlockData,
}

impl BlockHeaderData {
    /// Constructs a new `BlockHeaderData`.
    pub fn new(index: BlockIndex, data: BlockData) -> Self {
        Self { index, data }
    }

    /// Returns the stored [`BlockIndex`].
    pub fn index(&self) -> &BlockIndex {
        &self.index
    }

    /// Returns the stored [`BlockData`].
    pub fn data(&self) -> &BlockData {
        &self.data
    }
}

impl ZainoVersionedSerde for BlockHeaderData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.index.serialize(&mut *w)?;
        self.data.serialize(w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let index = BlockIndex::deserialize(&mut *r)?;
        let data = BlockData::deserialize(r)?;
        Ok(BlockHeaderData::new(index, data))
    }
}

/// Database wrapper for `Vec<Txid>`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxidList {
    /// Txids.
    txids: Vec<TransactionHash>,
}

impl TxidList {
    /// Creates a new `TxidList`.
    pub fn new(tx: Vec<TransactionHash>) -> Self {
        Self { txids: tx }
    }

    /// Returns a slice of the contained txids.
    pub fn txids(&self) -> &[TransactionHash] {
        &self.txids
    }
}

impl ZainoVersionedSerde for TxidList {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_vec(w, &self.txids, |w, h| h.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let tx = read_vec(r, |r| TransactionHash::deserialize(r))?;
        Ok(TxidList::new(tx))
    }
}

/// Wrapper for the list of transparent components of each transaction.
///
/// Each entry is `Some(TransparentCompactTx)` when the transaction **has**
/// a transparent part, and `None` when it does not.
///
/// This ensures 1-to-1 indexing with `TxidList`: element *i* matches txid *i*.
/// `None` keeps the index when the tx lacks this pool.
///
/// **Serialization layout for `TransparentTxList` (implements `ZainoVersionedSerde`)**
///
/// ┌──────────── byte 0 ─────────────┬────────── CompactSize ─────────────┬──────────── entries ───────────────┐
/// │ TransparentTxList version tag   │ num_txs (CompactSize) = N          │ [`Option<TransparentCompactTx>`; N]│
/// └─────────────────────────────────┴────────────────────────────────────┴────────────────────────────────────┘
///
/// Each `Option<TransparentCompactTx>` is serialized as:
///
/// ┌── 1 byte ──┬────────── TransparentCompactTx ─────────────┐
/// │   0 or 1   │ If Some: 1-byte version + body              │
/// └────────────┴─────────────────────────────────────────────┘
///
/// TransparentCompactTx:
/// ┌── version ─┬──── CompactSize vin_len ─┬──── vin entries ─────┬──── CompactSize vout_len ──┬──── vout entries ────┐
/// │    0x01    │ N1 (CompactSize)         │ [TxInCompact; N1]    │ N2 (CompactSize)           │ [TxOutCompact; N2]   │
/// └────────────┴──────────────────────────┴──────────────────────┴────────────────────────────┴──────────────────────┘
///
/// Each `TxInCompact` is serialized as:
/// ┌── version ─┬────────────── 36 bytes body ──────────────┐
/// │   0x01     │ 32-byte txid + 4-byte LE prevout_index    │
/// └────────────┴───────────────────────────────────────────┘
///
/// Each `TxOutCompact` is serialized as:
/// ┌── version ─┬────────────── 29 bytes body ──────────────┐
/// │   0x01     │ 8-byte LE value + 20-byte script_hash     │
/// │            │ + 1-byte script_type                      │
/// └────────────┴───────────────────────────────────────────┘
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TransparentTxList {
    /// Transparent transaction data.
    tx: Vec<Option<TransparentCompactTx>>,
}

impl TransparentTxList {
    /// Creates a new `TransparentTxList`.
    pub fn new(tx: Vec<Option<TransparentCompactTx>>) -> Self {
        Self { tx }
    }

    /// Returns the slice of optional transparent tx fragments.
    pub fn tx(&self) -> &[Option<TransparentCompactTx>] {
        &self.tx
    }
}

impl ZainoVersionedSerde for TransparentTxList {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_vec(w, &self.tx, |w, opt| {
            write_option(w, opt, |w, t| t.serialize(w))
        })
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let tx = read_vec(r, |r| {
            read_option(r, |r| TransparentCompactTx::deserialize(r))
        })?;
        Ok(TransparentTxList::new(tx))
    }
}

/// List of the Sapling component (if any) of every transaction in a block.
///
/// * Each element is `Some(SaplingCompactTx)` when that transaction **does**
///   contain Sapling data, or `None` when it does not.
///
/// This ensures 1-to-1 indexing with `TxidList`: element *i* matches txid *i*.
/// `None` keeps the index when the tx lacks this pool.
///
/// **Serialization layout for `SaplingTxList` (implements `ZainoVersionedSerde`)**
///
/// ┌──────────── byte 0 ─────────────┬────────── CompactSize ─────────────┬──────────── entries ───────────────┐
/// │ SaplingTxList version tag = 1   │ num_txs (CompactSize) = N          │ [`Option<SaplingCompactTx>`; N]    │
/// └─────────────────────────────────┴────────────────────────────────────┴────────────────────────────────────┘
///
/// Each `Option<SaplingCompactTx>` is serialized as:
///
/// ┌── 1 byte ──┬────────────── SaplingCompactTx ──────────────┐
/// │   0 or 1   │ If Some: 1-byte version + body               │
/// └────────────┴──────────────────────────────────────────────┘
///
/// SaplingCompactTx:
/// ┌── version ─┬──── 1 byte opt ─────┬──── CompactSize ──┬──── spend entries ─────────┬──── CompactSize ───┬──── output entries ─────────┐
/// │   0x01     │ 0 or 1 + i64 (value)│ N1 = num_spends   │ `[CompactSaplingSpend;N1]` │ N2 = num_outputs   │ `[CompactSaplingOutput;N2]` │
/// └────────────┴─────────────────────┴───────────────────┴────────────────────────────┴────────────────────┴─────────────────────────────┘
///
/// - The **Sapling value** is encoded as an `Option<i64>` using:
///     - 0 = None
///     - 1 = Some followed by 8-byte little-endian i64
///
/// Each `CompactSaplingSpend` is serialized as:
///
/// ┌── version ─┬────────────── 32 bytes ──────────────┐
/// │   0x01     │ 32-byte nullifier                    │
/// └────────────┴──────────────────────────────────────┘
///
/// Each `CompactSaplingOutput` is serialized as:
///
/// ┌── version ─┬────────────── 116 bytes ─────────────────────────────────────────────┐
/// │   0x01     │ 32-byte cmu + 32-byte ephemeral_key + 52-byte ciphertext             │
/// └────────────┴──────────────────────────────────────────────────────────────────────┘
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct SaplingTxList {
    tx: Vec<Option<SaplingCompactTx>>,
}

impl SaplingTxList {
    /// Creates a new [`SaplingTxList`]
    pub fn new(tx: Vec<Option<SaplingCompactTx>>) -> Self {
        Self { tx }
    }

    /// Returns transactions in this item.
    pub fn tx(&self) -> &[Option<SaplingCompactTx>] {
        &self.tx
    }
}

impl ZainoVersionedSerde for SaplingTxList {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_vec(w, &self.tx, |w, opt| {
            write_option(w, opt, |w, t| t.serialize(w))
        })
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let tx = read_vec(r, |r| read_option(r, |r| SaplingCompactTx::deserialize(r)))?;
        Ok(SaplingTxList::new(tx))
    }
}

/// List of the Orchard component (if any) of every transaction in a block.
///
/// * Each element is `Some(OrchardCompactTx)` when that transaction **does**
///   contain Sapling data, or `None` when it does not.
///
/// This ensures 1-to-1 indexing with `TxidList`: element *i* matches txid *i*.
/// `None` keeps the index when the tx lacks this pool.
///
/// **Serialization layout for `OrchardTxList` (implements `ZainoVersionedSerde`)**
///
/// ┌──────────── byte 0 ─────────────┬────────── CompactSize ─────────────┬──────────── entries ───────────────┐
/// │ OrchardTxList version tag = 1   │ num_txs (CompactSize) = N          │ [`Option<OrchardCompactTx>`; N]    │
/// └─────────────────────────────────┴────────────────────────────────────┴────────────────────────────────────┘
///
/// Each `Option<OrchardCompactTx>` is serialized as:
///
/// ┌── 1 byte ──┬────────────── OrchardCompactTx ───────────────┐
/// │   0 or 1   │ If Some: 1-byte version + body                │
/// └────────────┴───────────────────────────────────────────────┘
///
/// OrchardCompactTx:
/// ┌── version ─┬──── 1 byte opt ─────┬──── CompactSize ──────┬────────── action entries ─────────┐
/// │   0x01     │ 0 or 1 + i64 (value)│ N = num_actions       │ [CompactOrchardAction; N]         │
/// └────────────┴─────────────────────┴───────────────────────┴───────────────────────────────────┘
///
/// - The **Orchard value** is encoded as an `Option<i64>` using:
///     - 0 = None
///     - 1 = Some followed by 8-byte little-endian i64
///
/// Each `CompactOrchardAction` is serialized as:
///
/// ┌── version ─┬──────────── 148 bytes ─────────────────────────────────────────────────────────────┐
/// │   0x01     │ 32-byte nullifier + 32-byte cmx + 32-byte ephemeral_key + 52-byte ciphertext       │
/// └────────────┴────────────────────────────────────────────────────────────────────────────────────┘
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct OrchardTxList {
    tx: Vec<Option<OrchardCompactTx>>,
}

impl OrchardTxList {
    /// Creates a new [`OrchardTxList`]
    pub fn new(tx: Vec<Option<OrchardCompactTx>>) -> Self {
        Self { tx }
    }

    /// Returns transactions in this item.
    pub fn tx(&self) -> &[Option<OrchardCompactTx>] {
        &self.tx
    }
}

impl ZainoVersionedSerde for OrchardTxList {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_vec(w, &self.tx, |w, opt| {
            write_option(w, opt, |w, t| t.serialize(w))
        })
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let tx = read_vec(r, |r| read_option(r, |r| OrchardCompactTx::deserialize(r)))?;
        Ok(OrchardTxList::new(tx))
    }
}

// *** Custom serde based debug serialisation ***

#[cfg(test)]
/// utilities for serializing/deserializing nonstandard-sized arrays
pub mod serde_arrays {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialze an arbirtary fixed-size array
    pub fn serialize<const N: usize, S>(val: &[u8; N], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_bytes(val)
    }

    /// Deserialze an arbirtary fixed-size array
    pub fn deserialize<'de, const N: usize, D>(d: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let v: &[u8] = Deserialize::deserialize(d)?;
        v.try_into()
            .map_err(|_| serde::de::Error::custom(format!("invalid length for [u8; {N}]")))
    }
}
