//! Commitment tree types and utilities.
//!
//! This module contains types for managing Zcash commitment tree state, including
//! Merkle tree roots for Sapling and Orchard pools and combined tree metadata structures.
//!
//! Commitment trees track the existence of shielded notes in the Sapling and Orchard
//! shielded pools, enabling efficient zero-knowledge proofs and wallet synchronization.

use corez::io::{self, Read, Write};

use crate::codec::{
    read_fixed_le, read_option, read_u32_le, write_fixed_le, write_option, write_u32_le, DbCodec,
    FixedEncodedLen,
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

impl DbCodec for CommitmentTreeData {
    fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.roots.encode(w)?;
        self.sizes.encode(w)
    }

    fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let roots = CommitmentTreeRoots::decode(r)?;
        let sizes = CommitmentTreeSizes::decode(r)?;
        Ok(CommitmentTreeData::new(roots, sizes))
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
    /// Ironwood note-commitment tree root at this block.
    ironwood: Option<[u8; 32]>,
}

impl CommitmentTreeRoots {
    /// Reutns a new CommitmentTreeRoots instance.
    pub fn new(sapling: [u8; 32], orchard: [u8; 32], ironwood: Option<[u8; 32]>) -> Self {
        Self {
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Returns sapling commitment tree root.
    pub fn sapling(&self) -> &[u8; 32] {
        &self.sapling
    }

    /// returns orchard commitment tree root.
    pub fn orchard(&self) -> &[u8; 32] {
        &self.orchard
    }

    /// Returns the ironwood commitment tree root, which is `None` below NU6.3 activation or on a network that never activates it.
    pub(crate) fn ironwood(&self) -> &Option<[u8; 32]> {
        &self.ironwood
    }
}

impl DbCodec for CommitmentTreeRoots {
    fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(&mut *w, &self.sapling)?;
        write_fixed_le::<32, _>(&mut *w, &self.orchard)?;
        write_option(w, &self.ironwood, |w, v| write_fixed_le(w, v))
    }

    fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let sapling = read_fixed_le::<32, _>(&mut *r)?;
        let orchard = read_fixed_le::<32, _>(&mut *r)?;
        let ironwood = read_option(r, |r| read_fixed_le(r))?;
        Ok(CommitmentTreeRoots::new(sapling, orchard, ironwood))
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
    /// Total notes in Ironwood commitment tree.
    ironwood: u32,
}

impl CommitmentTreeSizes {
    /// Creates a new CompactSaplingSizes instance.
    pub fn new(sapling: u32, orchard: u32, ironwood: u32) -> Self {
        Self {
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Returns sapling commitment tree size
    pub fn sapling(&self) -> u32 {
        self.sapling
    }

    /// Returns orchard commitment tree size
    pub fn orchard(&self) -> u32 {
        self.orchard
    }

    /// Returns orchard commitment tree size
    pub fn ironwood(&self) -> u32 {
        self.ironwood
    }
}

impl DbCodec for CommitmentTreeSizes {
    fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u32_le(&mut *w, self.sapling)?;
        write_u32_le(&mut *w, self.orchard)?;
        write_u32_le(w, self.ironwood)
    }

    fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let sapling = read_u32_le(&mut *r)?;
        let orchard = read_u32_le(&mut *r)?;
        let ironwood = read_u32_le(r)?;
        Ok(CommitmentTreeSizes::new(sapling, orchard, ironwood))
    }
}

impl FixedEncodedLen for CommitmentTreeSizes {
    /// The record holds the Sapling, Orchard and Ironwood sizes as four bytes each.
    const ENCODED_LEN: usize = 4 + 4 + 4;
}
