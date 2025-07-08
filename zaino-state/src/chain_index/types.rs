//! Holds chain and block structs used internally by the ChainIndex.
//!
//! Held here to ensure serialisation consistency for ZainoDB.

use core2::io::{self, Read, Write};
use hex::{FromHex, ToHex};
use primitive_types::U256;
use std::fmt;

use crate::chain_index::encoding::{
    read_fixed_le, version, write_fixed_le, ZainoVersionedSerialise,
};

use super::encoding::{
    read_i64_le, read_option, read_u16_le, read_u32_be, read_u32_le, read_u64_le, read_vec,
    write_i64_le, write_option, write_u16_le, write_u32_be, write_u32_le, write_u64_le, write_vec,
};

// *** Key Objects ***

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

impl ZainoVersionedSerialise for Hash {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(Hash(bytes))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Block height.
///
/// NOTE: Encoded as 4-byte big-endian byte-string to ensure height ordering
/// for keys in Lexicographically sorted B-Tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Height(pub(crate) u32);

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

impl ZainoVersionedSerialise for Height {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Height must sort lexicographically - write **big-endian**
        write_u32_be(w, self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Height::try_from(raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Numerical index of subtree / shard roots.
///
/// NOTE: Encoded as 4-byte big-endian byte-string to ensure height ordering
/// for keys in Lexicographically sorted B-Tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Index(pub u32);

impl ZainoVersionedSerialise for Index {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Index must sort lexicographically - write **big-endian**
        write_u32_be(w, self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Ok(Index(raw))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// 20-byte hash (RIPEMD-160 or Blake2b-160) of a transparent output script.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct AddrScript([u8; 20]);

impl AddrScript {
    /// Create from raw bytes.
    pub fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Borrow the inner bytes.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Display for AddrScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl ToHex for &AddrScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.0.encode_hex()
    }
    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.0.encode_hex_upper()
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
    type Error = <[u8; 20] as FromHex>::Error;
    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        Ok(Self(<[u8; 20]>::from_hex(hex)?))
    }
}

impl From<[u8; 20]> for AddrScript {
    fn from(b: [u8; 20]) -> Self {
        Self(b)
    }
}
impl From<AddrScript> for [u8; 20] {
    fn from(a: AddrScript) -> Self {
        a.0
    }
}

impl ZainoVersionedSerialise for AddrScript {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<20, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<20, _>(r)?;
        Ok(AddrScript(bytes))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
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

    /// Returns the txid of the transaction being spent.
    pub fn prev_txid(&self) -> &[u8; 32] {
        &self.prev_txid
    }

    /// Returns the outpoint index withing the transaction.
    pub fn prev_index(&self) -> u32 {
        self.prev_index
    }
}

impl ZainoVersionedSerialise for Outpoint {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.prev_txid)?;
        write_u32_le(&mut w, self.prev_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let txid = read_fixed_le::<32, _>(&mut r)?;
        let index = read_u32_le(&mut r)?;
        Ok(Outpoint::new(txid, index))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

// *** Block Level Objects ***

/// Metadata about the block used to identify and navigate the blockchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct BlockIndex {
    /// The hash identifying this block uniquely.
    pub(super) hash: Hash,
    /// The hash of this block's parent block (previous block in chain).
    pub(super) parent_hash: Hash,
    /// The cumulative proof-of-work of the blockchain up to this block, used for chain selection.
    pub(super) chainwork: ChainWork,
    /// The height of this block if it's in the current best chain. None if it's part of a fork.
    pub(super) height: Option<Height>,
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

impl ZainoVersionedSerialise for BlockIndex {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        self.hash.serialize(&mut w)?;
        self.parent_hash.serialize(&mut w)?;
        self.chainwork.serialize(&mut w)?;

        write_option(&mut w, &self.height, |w, h| h.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let hash = Hash::deserialize(&mut r)?;
        let parent_hash = Hash::deserialize(&mut r)?;
        let chainwork = ChainWork::deserialize(&mut r)?;

        let height = read_option(&mut r, |r| Height::deserialize(r))?;

        Ok(BlockIndex::new(hash, parent_hash, chainwork, height))
    }

    /*──────── historic v1 helper ────────*/
    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for ChainWork {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(ChainWork(bytes))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
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
    pub(super) version: u32,
    /// Unix timestamp of when the block was mined (seconds since epoch).
    pub(super) time: i64,
    /// Merkle root hash of all transaction IDs in the block (used for quick tx inclusion proofs).
    pub(super) merkle_root: [u8; 32],
    /// Digest representing the block-commitments Merkle root (commitment to note states).
    /// - < V4: [`hashFinalSaplingRoot`] - Sapling note commitment tree root.
    /// - => V4: [`hashBlockCommitments`] - digest over hashLightClientRoot and hashAuthDataRoot.``
    pub(super) block_commitments: [u8; 32],
    /// Compact difficulty target used for proof-of-work and difficulty calculation.
    pub(super) bits: u32,
    /// Equihash nonse.
    pub(super) nonse: [u8; 32],
    /// Equihash solution
    pub(super) solution: EquihashSolution,
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
            nonse,
            solution,
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
        self.nonse
    }

    /// Returns Equihash Nonse.
    pub fn solution(&self) -> EquihashSolution {
        self.solution
    }
}

impl ZainoVersionedSerialise for BlockData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w; // re-borrow

        write_u32_le(&mut w, self.version)?;
        write_i64_le(&mut w, self.time)?;

        write_fixed_le::<32, _>(&mut w, &self.merkle_root)?;
        write_fixed_le::<32, _>(&mut w, &self.block_commitments)?;

        write_u32_le(&mut w, self.bits)?;
        write_fixed_le::<32, _>(&mut w, &self.nonse)?;

        self.solution.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
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

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for EquihashSolution {
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

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Pair of commitment-tree roots and their corresponding leaf counts
/// after the current block has been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CommitmentTreeData {
    roots: CommitmentTreeRoots,
    sizes: CommitmentTreeSizes,
}

impl CommitmentTreeData {
    /// Returns a new CommitmentTreeData instance.
    pub fn new(roots: CommitmentTreeRoots, sizes: CommitmentTreeSizes) -> Self {
        Self { roots, sizes }
    }

    /// Returns the commitment tree roots for the block.
    pub fn roots(&self) -> &CommitmentTreeRoots {
        &self.roots
    }

    /// Returns the commitment tree sizes for the block.
    pub fn sizes(&self) -> &CommitmentTreeSizes {
        &self.sizes
    }
}

impl ZainoVersionedSerialise for CommitmentTreeData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        self.roots.serialize(&mut w)?; // carries its own tag
        self.sizes.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let roots = CommitmentTreeRoots::deserialize(&mut r)?;
        let sizes = CommitmentTreeSizes::deserialize(&mut r)?;
        Ok(CommitmentTreeData::new(roots, sizes))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for CommitmentTreeRoots {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.sapling)?;
        write_fixed_le::<32, _>(&mut w, &self.orchard)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_fixed_le::<32, _>(&mut r)?;
        let orchard = read_fixed_le::<32, _>(&mut r)?;
        Ok(CommitmentTreeRoots::new(sapling, orchard))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for CommitmentTreeSizes {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u32_le(&mut w, self.sapling)?;
        write_u32_le(&mut w, self.orchard)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_u32_le(&mut r)?;
        let orchard = read_u32_le(&mut r)?;
        Ok(CommitmentTreeSizes::new(sapling, orchard))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Represents the indexing data of a single compact Zcash block used internally by Zaino.
/// Provides efficient indexing for blockchain state queries and updates.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct ChainBlock {
    /// Metadata and indexing information for this block.
    pub(super) index: BlockIndex,
    /// Essential header and metadata information for the block.
    pub(super) data: BlockData,
    /// Compact representations of transactions in this block.
    pub(super) transactions: Vec<CompactTxData>,
    /// Sapling and orchard commitment tree data for the chain
    /// *after this block has been applied.
    pub(super) commitment_tree_data: CommitmentTreeData,
}

impl ChainBlock {
    /// Creates a new `ChainBlock`.
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

        let sapling_commitment_tree_size = self.commitment_tree_data().sizes().sapling();
        let orchard_commitment_tree_size = self.commitment_tree_data().sizes().orchard();

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
            Hash::from(hash),
            Hash::from(parent_hash),
            chainwork,
            Some(height),
        );

        Ok(ChainBlock::new(index, block_data, tx, commitment_tree_data))
    }
}

// *** Transaction Objects ***

/// Compact indexed representation of a transaction within a block, supporting quick queries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactTxData {
    /// The index (position) of this transaction within its block (0-based).
    index: u64,
    /// Unique identifier (hash) of the transaction, used for lookup and indexing.
    txid: [u8; 32],
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
        txid: [u8; 32],
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
    pub fn txid(&self) -> &[u8; 32] {
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
impl TryFrom<(u64, zaino_fetch::chain::transaction::FullTransaction)> for CompactTxData {
    type Error = String;

    fn try_from(
        (index, tx): (u64, zaino_fetch::chain::transaction::FullTransaction),
    ) -> Result<Self, Self::Error> {
        let txid_vec = tx.tx_id();
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

        let vout: Vec<TxOutCompact> = tx
            .transparent_outputs()
            .into_iter()
            //TODO: We should error handle on these, a failure here should probably be
            // reacted to
            .filter_map(|(value, script_hash)| TxOutCompact::try_from((value, script_hash)).ok())
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

impl ZainoVersionedSerialise for TransparentCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_vec(&mut w, &self.vin, |w, txin| txin.serialize(w))?;
        write_vec(&mut w, &self.vout, |w, txout| txout.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let vin = read_vec(&mut r, |r| TxInCompact::deserialize(r))?;
        let vout = read_vec(&mut r, |r| TxOutCompact::deserialize(r))?;

        Ok(TransparentCompactTx::new(vin, vout))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

    /// Returns txid of the transaction that holds the output being sent.
    pub fn prevout_txid(&self) -> &[u8; 32] {
        &self.prevout_txid
    }

    /// Returns the index of the output being sent within the transaction.
    pub fn prevout_index(&self) -> u32 {
        self.prevout_index
    }
}

impl ZainoVersionedSerialise for TxInCompact {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.prevout_txid)?;
        write_u32_le(&mut w, self.prevout_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let txid = read_fixed_le::<32, _>(&mut r)?;
        let idx = read_u32_le(&mut r)?;
        Ok(TxInCompact::new(txid, idx))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for ScriptType {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&[*self as u8])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        ScriptType::try_from(b[0])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unknown ScriptType"))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

    fn try_from((value, script_hash): (u64, T)) -> Result<Self, Self::Error> {
        let script_hash_ref = script_hash.as_ref();
        if script_hash_ref.len() == 21 {
            let script_type = script_hash_ref[0];
            let mut hash_bytes = [0u8; 20];
            hash_bytes.copy_from_slice(&script_hash_ref[1..]);
            TxOutCompact::new(value, hash_bytes, script_type).ok_or(())
        } else {
            let mut fallback = [0u8; 20];
            let usable_len = script_hash_ref.len().min(20);
            fallback[..usable_len].copy_from_slice(&script_hash_ref[..usable_len]);
            TxOutCompact::new(value, fallback, ScriptType::NonStandard as u8).ok_or(())
        }
    }
}

impl ZainoVersionedSerialise for TxOutCompact {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u64_le(&mut w, self.value)?;
        write_fixed_le::<20, _>(&mut w, &self.script_hash)?;
        w.write_all(&[self.script_type])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let value = read_u64_le(&mut r)?;
        let script_hash = read_fixed_le::<20, _>(&mut r)?;

        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        TxOutCompact::new(value, script_hash, b[0])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid script_type"))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
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

impl ZainoVersionedSerialise for SaplingCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_option(&mut w, &self.value, |w, v| write_i64_le(w, *v))?;
        write_vec(&mut w, &self.spends, |w, s| s.serialize(w))?;
        write_vec(&mut w, &self.outputs, |w, o| o.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let value = read_option(&mut r, |r| read_i64_le(r))?;
        let spends = read_vec(&mut r, |r| CompactSaplingSpend::deserialize(r))?;
        let outputs = read_vec(&mut r, |r| CompactSaplingOutput::deserialize(r))?;

        Ok(SaplingCompactTx::new(value, spends, outputs))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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

impl ZainoVersionedSerialise for CompactSaplingSpend {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.nf)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(CompactSaplingSpend::new(read_fixed_le::<32, _>(r)?))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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
}

impl ZainoVersionedSerialise for CompactSaplingOutput {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.cmu)?;
        write_fixed_le::<32, _>(&mut w, &self.ephemeral_key)?;
        write_fixed_le::<52, _>(&mut w, &self.ciphertext)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let cmu = read_fixed_le::<32, _>(&mut r)?;
        let epk = read_fixed_le::<32, _>(&mut r)?;
        let ciphertext = read_fixed_le::<52, _>(&mut r)?;
        Ok(CompactSaplingOutput::new(cmu, epk, ciphertext))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
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

impl ZainoVersionedSerialise for OrchardCompactTx {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        write_option(&mut w, &self.value, |w, v| write_i64_le(w, *v))?;
        write_vec(&mut w, &self.actions, |w, a| a.serialize(w))
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;

        let value = read_option(&mut r, |r| read_i64_le(r))?;
        let actions = read_vec(&mut r, |r| CompactOrchardAction::deserialize(r))?;

        Ok(OrchardCompactTx::new(value, actions))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
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
}

impl ZainoVersionedSerialise for CompactOrchardAction {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.nullifier)?;
        write_fixed_le::<32, _>(&mut w, &self.cmx)?;
        write_fixed_le::<32, _>(&mut w, &self.ephemeral_key)?;
        write_fixed_le::<52, _>(&mut w, &self.ciphertext)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let nf = read_fixed_le::<32, _>(&mut r)?;
        let cmx = read_fixed_le::<32, _>(&mut r)?;
        let epk = read_fixed_le::<32, _>(&mut r)?;
        let ctxt = read_fixed_le::<52, _>(&mut r)?;
        Ok(CompactOrchardAction::new(nf, cmx, epk, ctxt))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Identifies a transaction by its (block_position, tx_position) pair,
/// used to locate transactions within Zaino's internal DB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct TxIndex {
    /// Block Height in chain.
    block_index: u32,
    /// Transaction index in block.
    tx_index: u16,
}

impl TxIndex {
    /// Creates a new TxIndex instance.
    pub fn new(block_index: u32, tx_index: u16) -> Self {
        Self {
            block_index,
            tx_index,
        }
    }

    /// Returns the block height held in the TxIndex.
    pub fn block_index(&self) -> u32 {
        self.block_index
    }

    /// Returns the transaction index held in the TxIndex.
    pub fn tx_index(&self) -> u16 {
        self.tx_index
    }
}

impl ZainoVersionedSerialise for TxIndex {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u32_le(&mut w, self.block_index)?;
        write_u16_le(&mut w, self.tx_index)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let blk = read_u32_le(&mut r)?;
        let tx = read_u16_le(&mut r)?;
        Ok(TxIndex::new(blk, tx))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// Single transparent-address activity record (input or output).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct AddrHistRecord {
    tx_index: TxIndex,
    out_index: u16,
    value: u64,
    flags: u8,
}

/* ----- flag helpers ----- */
impl AddrHistRecord {
    /// Flag mask for is_mined.
    pub const FLAG_MINED: u8 = 0x01;

    /// Flag mask for is_spent.
    pub const FLAG_SPENT: u8 = 0x02;

    /// Flag mask for is_input.
    pub const FLAG_IS_INPUT: u8 = 0x04;

    /// Creatues a new AddrHistRecord instance.
    pub fn new(tx_index: TxIndex, out_index: u16, value: u64, flags: u8) -> Self {
        Self {
            tx_index,
            out_index,
            value,
            flags,
        }
    }

    /// Returns the TxIndex in this record.
    pub fn tx_index(&self) -> TxIndex {
        self.tx_index
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

impl ZainoVersionedSerialise for AddrHistRecord {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        self.tx_index.serialize(&mut w)?;
        write_u16_le(&mut w, self.out_index)?;
        write_u64_le(&mut w, self.value)?;
        w.write_all(&[self.flags])
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let tx_index = TxIndex::deserialize(&mut r)?;
        let out_index = read_u16_le(&mut r)?;
        let value = read_u64_le(&mut r)?;

        let mut flag = [0u8; 1];
        r.read_exact(&mut flag)?;

        Ok(AddrHistRecord::new(tx_index, out_index, value, flag[0]))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

/// AddrHistRecord database byte array.
///
/// EXACTLY 17 bytes – duplicate value in `addr_hist` DBI.
///
/// Layout (all big-endian except `value`):
/// [0..4]  height
/// [4..6]  tx_index
/// [6..8]  vout
/// [8]     flags
/// [9..17] value  (little-endian, matches Zcashd)
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct AddrEventBytes([u8; 17]);

impl AddrEventBytes {
    const LEN: usize = 17;

    /// Create an [`AddrEventBytes`] from an [`AddrHistRecord`].
    #[allow(dead_code)]
    pub(crate) fn from_record(rec: &AddrHistRecord) -> Self {
        use byteorder::{BigEndian, ByteOrder, LittleEndian};

        let mut buf = [0u8; Self::LEN];
        BigEndian::write_u32(&mut buf[0..4], rec.tx_index.block_index);
        BigEndian::write_u16(&mut buf[4..6], rec.tx_index.tx_index);
        BigEndian::write_u16(&mut buf[6..8], rec.out_index);
        buf[8] = rec.flags;
        LittleEndian::write_u64(&mut buf[9..17], rec.value);
        Self(buf)
    }

    /// Create an [`AddrHistRecord`] from an [`AddrEventBytes`].
    #[allow(dead_code)]
    pub(crate) fn as_record(&self) -> AddrHistRecord {
        use byteorder::{BigEndian, ByteOrder, LittleEndian};
        let b = &self.0;
        AddrHistRecord {
            tx_index: TxIndex {
                block_index: BigEndian::read_u32(&b[0..4]),
                tx_index: BigEndian::read_u16(&b[4..6]),
            },
            out_index: BigEndian::read_u16(&b[6..8]),
            flags: b[8],
            value: LittleEndian::read_u64(&b[9..17]),
        }
    }

    /// Borrow the raw bytes.
    #[allow(dead_code)]
    pub(crate) fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl ZainoVersionedSerialise for AddrEventBytes {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<17, _>(w, &self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(AddrEventBytes(read_fixed_le::<17, _>(r)?))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
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

impl ZainoVersionedSerialise for ShardRoot {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.hash)?;
        write_fixed_le::<32, _>(&mut w, &self.final_block_hash)?;
        write_u32_le(&mut w, self.final_block_height)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let hash = read_fixed_le::<32, _>(&mut r)?;
        let final_block_hash = read_fixed_le::<32, _>(&mut r)?;
        let final_block_height = read_u32_le(&mut r)?;
        Ok(ShardRoot::new(hash, final_block_hash, final_block_height))
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_latest(r)
    }
}

// *** Wrapper Objects ***

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
