//! Commitment tree types and utilities.
//!
//! This module contains types for managing Zcash commitment tree state, including
//! Merkle tree roots for Sapling and Orchard pools and combined tree metadata structures.
//!
//! Commitment trees track the existence of shielded notes in the Sapling and Orchard
//! shielded pools, enabling efficient zero-knowledge proofs and wallet synchronization.

use core2::io::{self, Read, Write};

use crate::chain_index::encoding::{
    read_fixed_le, read_u32_le, version, write_fixed_le, write_u32_le, FixedEncodedLen,
    ZainoVersionedSerde,
};

/// Holds commitment tree metadata (roots and sizes) for a block.
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

impl ZainoVersionedSerde for CommitmentTreeData {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        self.roots.serialize(&mut w)?; // carries its own tag
        self.sizes.serialize(&mut w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let roots = CommitmentTreeRoots::deserialize(&mut r)?;
        let sizes = CommitmentTreeSizes::deserialize(&mut r)?;
        Ok(CommitmentTreeData::new(roots, sizes))
    }
}

/// CommitmentTreeData: 74 bytes total
impl FixedEncodedLen for CommitmentTreeData {
    // 1 byte tag + 64 body for roots
    // + 1 byte tag +  8 body for sizes
    const ENCODED_LEN: usize =
        (CommitmentTreeRoots::ENCODED_LEN + 1) + (CommitmentTreeSizes::ENCODED_LEN + 1);
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

impl ZainoVersionedSerde for CommitmentTreeRoots {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.sapling)?;
        write_fixed_le::<32, _>(&mut w, &self.orchard)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_fixed_le::<32, _>(&mut r)?;
        let orchard = read_fixed_le::<32, _>(&mut r)?;
        Ok(CommitmentTreeRoots::new(sapling, orchard))
    }
}

/// CommitmentTreeRoots: 64 bytes total
impl FixedEncodedLen for CommitmentTreeRoots {
    /// 32 byte hash + 32 byte hash.
    const ENCODED_LEN: usize = 32 + 32;
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

impl ZainoVersionedSerde for CommitmentTreeSizes {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u32_le(&mut w, self.sapling)?;
        write_u32_le(&mut w, self.orchard)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_u32_le(&mut r)?;
        let orchard = read_u32_le(&mut r)?;
        Ok(CommitmentTreeSizes::new(sapling, orchard))
    }
}

/// CommitmentTreeSizes: 8 bytes total
impl FixedEncodedLen for CommitmentTreeSizes {
    /// 4 byte LE int32 + 4 byte LE int32
    const ENCODED_LEN: usize = 4 + 4;
}
