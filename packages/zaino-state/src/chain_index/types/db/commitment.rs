//! Commitment tree types and utilities.
//!
//! This module contains types for managing Zcash commitment tree state, including
//! Merkle tree roots for Sapling and Orchard pools and combined tree metadata structures.
//!
//! Commitment trees track the existence of shielded notes in the Sapling and Orchard
//! shielded pools, enabling efficient zero-knowledge proofs and wallet synchronization.

use corez::io::{self, Read, Write};

use crate::{
    chain_index::encoding::{
        read_fixed_le, read_u32_le, version, write_fixed_le, write_u32_le, FixedEncodedLen,
        ZainoVersionedSerde,
    },
    read_option, write_option,
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
    const VERSION: u8 = version::V2;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v2(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v2(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        self.roots.serialize_with_version(&mut w, 1)?;
        self.sizes.serialize_with_version(&mut w, 1)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let roots = CommitmentTreeRoots::deserialize(&mut r)?;
        let sizes = CommitmentTreeSizes::deserialize(&mut r)?;
        Ok(CommitmentTreeData::new(roots, sizes))
    }

    fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        self.roots.serialize_with_version(&mut w, 2)?;
        self.sizes.serialize_with_version(&mut w, 2)
    }

    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let roots = CommitmentTreeRoots::deserialize(&mut r)?;
        let sizes = CommitmentTreeSizes::deserialize(&mut r)?;
        Ok(CommitmentTreeData::new(roots, sizes))
    }
}

impl FixedEncodedLen for CommitmentTreeData {
    /// Returns the fixed encoded body length for a specific
    /// `CommitmentTreeData` version.
    ///
    /// v1 is fixed-length:
    /// - versioned `CommitmentTreeRoots` v1
    /// - versioned `CommitmentTreeSizes` v1
    ///
    /// v2 is variable-length because `CommitmentTreeRoots` v2 contains an
    /// `Option<[u8; 32]>`.
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(
                CommitmentTreeRoots::versioned_len(version::V1)?
                    + CommitmentTreeSizes::versioned_len(version::V1)?,
            ),
            version::V2 => None,
            _ => None,
        }
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

    /// returns orchard commitment tree root.
    /// No production reader consumes the stored ironwood root yet; the regression test
    /// for its None-preservation does. Un-gate when a production consumer appears.
    #[cfg(test)]
    pub(crate) fn ironwood(&self) -> &Option<[u8; 32]> {
        &self.ironwood
    }
}

impl ZainoVersionedSerde for CommitmentTreeRoots {
    const VERSION: u8 = version::V2;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v2(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v2(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.sapling)?;
        write_fixed_le::<32, _>(&mut w, &self.orchard)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_fixed_le::<32, _>(&mut r)?;
        let orchard = read_fixed_le::<32, _>(&mut r)?;
        Ok(CommitmentTreeRoots::new(sapling, orchard, None))
    }

    fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_fixed_le::<32, _>(&mut w, &self.sapling)?;
        write_fixed_le::<32, _>(&mut w, &self.orchard)?;
        write_option(&mut w, &self.ironwood, |w, v| write_fixed_le(w, v))
    }

    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_fixed_le::<32, _>(&mut r)?;
        let orchard = read_fixed_le::<32, _>(&mut r)?;
        let ironwood = read_option(&mut r, |r| read_fixed_le(r))?;

        Ok(CommitmentTreeRoots::new(sapling, orchard, ironwood))
    }
}

impl FixedEncodedLen for CommitmentTreeRoots {
    /// Returns the fixed encoded body length for a specific
    /// `CommitmentTreeRoots` version.
    ///
    /// v1 is fixed-length:
    /// - 32 bytes Sapling root
    /// - 32 bytes Orchard root
    ///
    /// v2 is variable-length because `ironwood: Option<[u8; 32]>` encodes as:
    /// - 1 byte option tag
    /// - plus 32 bytes when present
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(32 + 32),
            version::V2 => None,
            _ => None,
        }
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

impl ZainoVersionedSerde for CommitmentTreeSizes {
    const VERSION: u8 = version::V2;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v2(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v2(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u32_le(&mut w, self.sapling)?;
        write_u32_le(&mut w, self.orchard)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_u32_le(&mut r)?;
        let orchard = read_u32_le(&mut r)?;
        Ok(CommitmentTreeSizes::new(sapling, orchard, 0))
    }

    fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u32_le(&mut w, self.sapling)?;
        write_u32_le(&mut w, self.orchard)?;
        write_u32_le(&mut w, self.ironwood)
    }

    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let sapling = read_u32_le(&mut r)?;
        let orchard = read_u32_le(&mut r)?;
        let ironwood = read_u32_le(&mut r)?;
        Ok(CommitmentTreeSizes::new(sapling, orchard, ironwood))
    }
}

impl FixedEncodedLen for CommitmentTreeSizes {
    /// Returns the fixed encoded body length for a specific
    /// `CommitmentTreeSizes` version.
    ///
    /// v1 is fixed-length:
    /// - 4 bytes Sapling size
    /// - 4 bytes Orchard size
    ///
    /// v2 is also fixed-length:
    /// - 4 bytes Sapling size
    /// - 4 bytes Orchard size
    /// - 4 bytes Ironwood size
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(4 + 4),
            version::V2 => Some(4 + 4 + 4),
            _ => None,
        }
    }
}
