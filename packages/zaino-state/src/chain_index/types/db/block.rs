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

use corez::io::{self, Read, Write};

use crate::chain_index::{
    encoding::{read_option, version, write_option, ZainoVersionedSerde},
    types::{BlockContext, BlockHash, BlockIndex, ChainWork, Height},
};

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
    pub(super) chainwork: ChainWork,
    pub(super) height: Height,
}

impl PersistentBlockContext {
    /// Build a `PersistentBlockContext` from a business-layer `BlockContext`,
    /// flattening the nested `(height, hash)` primitive into the
    /// persistence-shape fields.
    ///
    /// Replaces `impl From<&BlockContext>`. The named method makes the
    /// direction (business → persistence) and the boundary it crosses
    /// unambiguous at every call site.
    pub(super) fn from_business(context: &BlockContext) -> Self {
        Self {
            hash: context.index.hash,
            parent_hash: context.parent_hash,
            chainwork: context.chainwork,
            height: context.height(),
        }
    }

    /// Consume this `PersistentBlockContext` and produce the business-layer
    /// `BlockContext`. This conversion is the on-disk → business validation
    /// boundary — any check that must hold for a `BlockContext` to exist
    /// should live here.
    ///
    /// Replaces `impl From<PersistentBlockContext> for BlockContext`.
    pub(super) fn into_business(self) -> BlockContext {
        BlockContext {
            index: BlockIndex {
                height: self.height,
                hash: self.hash,
            },
            parent_hash: self.parent_hash,
            chainwork: self.chainwork,
        }
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
        let chainwork = ChainWork::deserialize(&mut r)?;
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
        let chainwork = ChainWork::deserialize(&mut r)?;
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
            ChainWork::from_u256(0x0123_4567u64.into()),
            Height(0x0dec_0de0),
        );
        let persisted = PersistentBlockContext::from_business(&bctx);
        let back = persisted.into_business();
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
