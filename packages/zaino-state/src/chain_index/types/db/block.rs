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

impl From<&BlockContext> for PersistentBlockContext {
    fn from(c: &BlockContext) -> Self {
        Self {
            hash: c.index.hash,
            parent_hash: c.parent_hash,
            chainwork: c.chainwork,
            height: c.index.height,
        }
    }
}

impl From<PersistentBlockContext> for BlockContext {
    fn from(p: PersistentBlockContext) -> Self {
        Self {
            index: BlockIndex {
                height: p.height,
                hash: p.hash,
            },
            parent_hash: p.parent_hash,
            chainwork: p.chainwork,
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
    use crate::chain_index::types::{BlockHash, ChainWork, Height};

    /// `BlockContext → PersistentBlockContext → BlockContext` is identity.
    ///
    /// Fails if the `From` impls ever drift into lossy or non-total
    /// conversions — catches a class of bug where a deserialised record
    /// cannot be mapped back to the business-layer type.
    #[test]
    fn block_context_round_trips_through_persistent() {
        let bctx = BlockContext::new(
            BlockHash::from([0x11; 32]),
            BlockHash::from([0x22; 32]),
            ChainWork::from_u256(0x0123_4567u64.into()),
            Height(0x0dec_0de_0),
        );
        let persisted = PersistentBlockContext::from(&bctx);
        let back: BlockContext = persisted.into();
        assert_eq!(bctx, back);
    }
}
