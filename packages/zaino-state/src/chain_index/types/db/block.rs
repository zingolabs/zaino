//! Block-related database-serializable types.
//!
//! Contains types for block data that implement `ZainoVersionedSerde`:
//! - `PersistentBlockContext` (module-private; the DB serde boundary for
//!   the business-layer [`BlockContext`])
//! - BlockHash
//! - BlockData
//! - BlockHeaderData
//! - IndexedBlock
//! - EquihashSolution
//! - ChainWork
//!
//! The business-layer container [`BlockContext`] itself is **not** a DB
//! type — it has no serde impl. It lives in `types/block_context.rs`.
//! The `From` conversions between `BlockContext` and
//! `PersistentBlockContext` are defined here, alongside PBC.

use std::num::NonZeroU128;

use corez::io::{self, Read, Write};

use crate::chain_index::{
    encoding::{
        read_fixed_le, read_option, read_u32_le, version, write_fixed_le, write_option,
        write_u32_le, FixedEncodedLen, ZainoVersionedSerde,
    },
    types::{
        BlockContext, BlockHash, BlockIndex, ChainWork, CompactDifficulty, Height,
    },
};

/// Database-adjacent persistence shape for [`ChainWork`].
///
/// On disk the value is stored as a 32-byte little-endian unsigned integer
/// (the original U256 format). On the way back to the business layer the
/// upper 16 bytes must be zero (the value must fit in `u128`) and the lower
/// 16 bytes must be nonzero.
#[derive(Debug)]
pub(super) struct PersistentChainWork([u8; 32]);

impl PersistentChainWork {
    pub(super) fn from_business(cw: &ChainWork) -> Self {
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&cw.as_non_zero_u128().get().to_le_bytes());
        Self(buf)
    }

    pub(super) fn into_business(self) -> io::Result<ChainWork> {
        // Upper 16 bytes must be zero (value must fit in u128).
        if self.0[16..] != [0u8; 16] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chainwork exceeds u128 range",
            ));
        }
        let mut le_bytes = [0u8; 16];
        le_bytes.copy_from_slice(&self.0[..16]);
        let value = u128::from_le_bytes(le_bytes);
        NonZeroU128::new(value)
            .map(ChainWork::new)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "chainwork is zero"))
    }
}

impl ZainoVersionedSerde for PersistentChainWork {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.0)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let bytes = read_fixed_le::<32, _>(r)?;
        Ok(Self(bytes))
    }
}

impl FixedEncodedLen for PersistentChainWork {
    const ENCODED_LEN: usize = 32;
}

/// Database-adjacent persistence shape for [`CompactDifficulty`].
///
/// Stores the raw `u32` nBits value. Validation happens in `into_business`.
#[derive(Debug)]
pub(super) struct PersistentCompactDifficulty(u32);

impl PersistentCompactDifficulty {
    pub(super) fn from_business(cd: &CompactDifficulty) -> Self {
        Self(cd.as_bits())
    }

    pub(super) fn into_business(self) -> io::Result<CompactDifficulty> {
        CompactDifficulty::try_from_bits(self.0).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e)
        })
    }
}

impl ZainoVersionedSerde for PersistentCompactDifficulty {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u32_le(w, self.0)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let bits = read_u32_le(r)?;
        Ok(Self(bits))
    }
}

impl FixedEncodedLen for PersistentCompactDifficulty {
    const ENCODED_LEN: usize = 4;
}

/// Database-adjacent persistence shape for [`BlockContext`].
///
/// Its sole responsibility is serde at the storage boundary. Kept
/// `pub(super)` so its sibling consumers in `legacy.rs`
/// (`IndexedBlock`, `BlockHeaderData`) can reach it without it leaking
/// into the crate's public surface — every round-trip between a
/// `BlockContext` and on-disk bytes goes through this type via the `From`
/// conversions below.
///
/// The field layout and order match the on-disk v1/v2 wire format exactly.
#[derive(Debug)]
pub(super) struct PersistentBlockContext {
    pub(super) hash: BlockHash,
    pub(super) parent_hash: BlockHash,
    pub(super) chainwork: PersistentChainWork,
    pub(super) height: Height,
}

impl PersistentBlockContext {
    pub(super) fn from_business(context: &BlockContext) -> Self {
        Self {
            hash: context.index.hash,
            parent_hash: context.parent_hash,
            chainwork: PersistentChainWork::from_business(&context.chainwork),
            height: context.height(),
        }
    }

    pub(super) fn into_business(self) -> io::Result<BlockContext> {
        Ok(BlockContext {
            index: BlockIndex {
                height: self.height,
                hash: self.hash,
            },
            parent_hash: self.parent_hash,
            chainwork: self.chainwork.into_business()?,
        })
    }
}

impl ZainoVersionedSerde for PersistentBlockContext {
    const VERSION: u8 = version::V2;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v2(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v2(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        self.hash.serialize_with_version(&mut w, 1)?;
        self.parent_hash.serialize_with_version(&mut w, 1)?;
        self.chainwork.serialize_with_version(&mut w, 1)?;
        write_option(&mut w, &Some(self.height), |w, h| {
            h.serialize_with_version(w, 1)
        })
    }

    fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;

        self.hash.serialize_with_version(&mut w, 1)?;
        self.parent_hash.serialize_with_version(&mut w, 1)?;
        self.chainwork.serialize_with_version(&mut w, 1)?;
        self.height.serialize_with_version(&mut w, 1)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let hash = BlockHash::deserialize(&mut r)?;
        let parent_hash = BlockHash::deserialize(&mut r)?;
        let chainwork = PersistentChainWork::deserialize(&mut r)?;
        let height =
            read_option(&mut r, |r| Height::deserialize(r))?.expect("blocks always have height");
        Ok(Self {
            hash,
            parent_hash,
            chainwork,
            height,
        })
    }

    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let hash = BlockHash::deserialize(&mut r)?;
        let parent_hash = BlockHash::deserialize(&mut r)?;
        let chainwork = PersistentChainWork::deserialize(&mut r)?;
        let height = Height::deserialize(&mut r)?;
        Ok(Self {
            hash,
            parent_hash,
            chainwork,
            height,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the `BlockContext` ↔ `PersistentBlockContext` boundary.
    //!
    //! `PersistentBlockContext` is module-private by design, so these tests
    //! live alongside its definition.

    use std::num::NonZeroU128;

    use super::{BlockContext, PersistentBlockContext};
    use crate::chain_index::tests::types::{canonical_blockheaderdata, expected_v2_bytes};
    use crate::chain_index::types::{BlockHash, BlockIndex, ChainWork, Height};
    use crate::{BlockHeaderData, ZainoVersionedSerde as _};

    /// `BlockContext → PersistentBlockContext → BlockContext` is identity.
    ///
    /// Fails if the `from_business` / `into_business` conversions ever drift
    /// into lossy or non-total mappings — catches a class of bug where a
    /// deserialised record cannot be mapped back to the business-layer type.
    #[test]
    fn block_context_round_trips_through_persistent() {
        let bctx = BlockContext::new(
            BlockHash::from([0x11; 32]),
            BlockHash::from([0x22; 32]),
            ChainWork::new(NonZeroU128::new(0x0123_4567u128).expect("nonzero")),
            Height(0x0dec_0de0),
        );
        let persisted = PersistentBlockContext::from_business(&bctx);
        let back = persisted.into_business().expect("valid chainwork");
        assert_eq!(bctx, back);
    }

    /// Cross-boundary tour for the `(height, hash)` slice:
    ///
    /// ```text
    ///   DB bytes → BlockHeaderData → BlockContext → BlockIndex →
    ///   proto::BlockId → BlockIndex'
    /// ```
    ///
    /// Assertions:
    ///   1. Decoding the canonical V2 golden bytes produces the canonical
    ///      `BlockHeaderData` (DB serde + DB→business crossing intact).
    ///   2. Re-encoding yields the same bytes byte-for-byte (the DB-side
    ///      round-trip is whole; no encoder drift hidden behind this test).
    ///   3. The `BlockIndex` slice survives the wire round-trip
    ///      (`to_wire` / `try_from_wire`) unchanged.
    ///
    /// Pair with `block_index_round_trips_through_wire` in `types/wire.rs`:
    /// if the narrow wire test passes but this cross-boundary test fails,
    /// the bug lives in the DB layer or at the DB↔business crossing, not in
    /// the wire conversion itself.
    ///
    /// A full `BlockContext` round-trip via wire is intentionally NOT
    /// attempted — `proto::BlockId` carries only `(height, hash)`, dropping
    /// `parent_hash` and `chainwork`. That asymmetry is the point: the wire
    /// protocol is narrower than the business type, by design.
    #[test]
    fn block_index_slice_round_trips_across_boundaries() {
        let original_bytes = expected_v2_bytes();

        // DB bytes → business.
        let header =
            BlockHeaderData::from_bytes(&original_bytes).expect("decode canonical V2 bytes");
        assert_eq!(header, canonical_blockheaderdata());

        // DB side is whole: re-encoding produces identical bytes.
        let re_encoded = header.to_bytes().expect("re-encode BlockHeaderData");
        assert_eq!(re_encoded, original_bytes);

        // Extract the (height, hash) slice.
        let index: BlockIndex = header.context.index;

        // Business → wire → business.
        let wire = index.to_wire();
        let recovered = BlockIndex::try_from_wire(wire).expect("valid wire shape");
        assert_eq!(index, recovered);
    }
}
