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
    types::{BlockContext, BlockHash, BlockIndex, ChainWork, CompactDifficulty, Height},
};

/// Database-adjacent persistence shape for [`ChainWork`].
///
/// On disk the value is a 32-byte **big-endian** unsigned integer — the format
/// established by the original `ChainWork([u8; 32])`, which serialized through
/// `U256::to_big_endian`/`from_big_endian`. The byte order must match that
/// format exactly to stay compatible with existing v1 databases.
///
/// Coming back to the business layer the value must fit in `u128`, so the
/// **high-order** 16 bytes (`[..16]`, big-endian most-significant) must be zero
/// and the **low-order** 16 bytes (`[16..]`) hold the nonzero `u128`.
#[derive(Debug)]
pub(super) struct PersistentChainWork([u8; 32]);

impl PersistentChainWork {
    pub(super) fn from_business(cw: &ChainWork) -> Self {
        let mut buf = [0u8; 32];
        // Big-endian: the value occupies the low-order (last) 16 bytes.
        buf[16..].copy_from_slice(&cw.as_non_zero_u128().get().to_be_bytes());
        Self(buf)
    }

    pub(super) fn into_business(self) -> io::Result<ChainWork> {
        // Big-endian: the high-order 16 bytes must be zero for the value to fit u128.
        if self.0[..16] != [0u8; 16] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chainwork exceeds u128 range",
            ));
        }
        let mut be_bytes = [0u8; 16];
        be_bytes.copy_from_slice(&self.0[16..]);
        let value = u128::from_be_bytes(be_bytes);
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

/// Fixed-length encoding metadata for `PersistentChainWork`.
///
/// v1 consists of a single 32-byte value.
impl FixedEncodedLen for PersistentChainWork {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(32),
            _ => None,
        }
    }
}

/// Database-adjacent persistence shape for [`CompactDifficulty`].
///
/// Stores the raw `u32` nBits value. Validation happens in `into_business`.
#[derive(Debug)]
pub(super) struct PersistentCompactDifficulty(u32);

#[expect(dead_code, reason = "will be used by versioned DB schema types")]
impl PersistentCompactDifficulty {
    pub(super) fn from_business(cd: &CompactDifficulty) -> Self {
        Self(cd.as_bits())
    }

    pub(super) fn into_business(self) -> io::Result<CompactDifficulty> {
        CompactDifficulty::try_from_bits(self.0)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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

/// Fixed-length encoding metadata for `PersistentCompactDifficulty`.
///
/// v1 consists of a single 4-byte LE u32.
impl FixedEncodedLen for PersistentCompactDifficulty {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(4),
            _ => None,
        }
    }
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

    use super::{BlockContext, PersistentBlockContext, PersistentChainWork};
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

    /// Regression for the byte-order bug that broke `load_db_backend_from_file`
    /// and every existing v1 DB: the on-disk chainwork format is 32-byte
    /// **big-endian** (the original `ChainWork([u8; 32])` via
    /// `U256::to_big_endian`). Decoding it little-endian pushes a small value's
    /// bytes into the high half and spuriously rejects it as ">u128".
    ///
    /// These are the *exact* bytes in the committed `v1_test_db` fixture: a
    /// regtest cumulative chainwork of 17.
    #[test]
    fn decodes_original_big_endian_chainwork() {
        let mut on_disk = [0u8; 32];
        on_disk[31] = 0x11; // big-endian 17: value in the least-significant byte
        let cw = PersistentChainWork(on_disk)
            .into_business()
            .expect("big-endian on-disk chainwork must decode");
        assert_eq!(cw.as_non_zero_u128().get(), 17);
    }

    /// Verbatim recovery of the pre-#1313 on-disk `ChainWork` encoder — the
    /// authority for the v1 **big-endian** byte order. Extracted with
    /// `git show 5e4dae4a^:packages/zaino-state/src/chain_index/types/db/legacy.rs`
    /// (5e4dae4a is the #1313 commit that deleted it), kept so the current
    /// `PersistentChainWork` is diffed against the real original rather than a
    /// hand-reconstruction. Only the encoder surface is retained; `U256` is
    /// fully qualified rather than imported.
    mod legacy_chainwork_reference {
        /// Cumulative proof-of-work of the chain,
        /// stored as a **big-endian** 256-bit unsigned integer.
        //
        // DOCUMENTATION BUG — the likely root of #1313: this struct doc correctly
        // says *big-endian*, but the original's `impl FixedEncodedLen for ChainWork`
        // was annotated `/// 32 bytes, LE`, and the serializer it uses is named
        // `write_fixed_le`. That "LE" label over big-endian bytes is what plausibly
        // led #1313 to re-encode the field little-endian.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(super) struct ChainWork([u8; 32]);

        impl ChainWork {
            /// Returns ChainWork as a U256.
            pub(super) fn to_u256(&self) -> primitive_types::U256 {
                primitive_types::U256::from_big_endian(&self.0)
            }

            /// Builds a ChainWork from a U256.
            pub(super) fn from_u256(value: primitive_types::U256) -> Self {
                let buf: [u8; 32] = value.to_big_endian();
                ChainWork(buf)
            }

            /// Returns ChainWork bytes.
            pub(super) fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }
    }

    /// Both directions of the cross-encoder equivalence for one value: the
    /// current encoder reproduces the recovered original's big-endian on-disk
    /// bytes, and the current decoder reads those original bytes back to the
    /// same value.
    fn assert_encoders_agree(value: u128) {
        let cw = ChainWork::new(NonZeroU128::new(value).expect("nonzero"));
        let original =
            legacy_chainwork_reference::ChainWork::from_u256(primitive_types::U256::from(value));
        let original_bytes = *original.as_bytes();

        // Encode: the current encoder reproduces the original's big-endian bytes.
        assert_eq!(
            PersistentChainWork::from_business(&cw).0,
            original_bytes,
            "encode mismatch at {value:#034x}",
        );
        // Decode both ways agree with the encoding: the current decoder reads the
        // original bytes back to `value`, and the recovered original decoder
        // (`to_u256`) reads its own bytes back to the same value.
        assert_eq!(
            PersistentChainWork(original_bytes)
                .into_business()
                .expect("original on-disk bytes must decode"),
            cw,
            "decode mismatch at {value:#034x}",
        );
        assert_eq!(original.to_u256(), primitive_types::U256::from(value));
    }

    /// Cross-encoder equivalence across the whole domain where the two encoders
    /// are *intended* to match — every nonzero value the `u128` `ChainWork` can
    /// hold. This is the check #1313 lacked: red against its little-endian
    /// encoder, green only when the byte order matches the established
    /// big-endian format.
    ///
    /// Deterministic coverage of every structurally-distinct case — the
    /// extremes, each power of two, each byte position fully set, and cumulative
    /// low-byte fills — so any per-byte transposition or width error is caught.
    #[test]
    fn new_encoder_matches_recovered_original_exhaustively() {
        assert_encoders_agree(1); // minimum nonzero
        assert_encoders_agree(17); // the v1_test_db fixture value
        assert_encoders_agree(u128::MAX); // maximum
        for bit in 0..128 {
            assert_encoders_agree(1u128 << bit);
        }
        for byte in 0..16 {
            assert_encoders_agree(0xffu128 << (8 * byte));
            assert_encoders_agree(u128::MAX >> (8 * byte));
        }
    }

    proptest::proptest! {
        /// Random coverage across the whole nonzero `u128` range, complementing
        /// the deterministic boundary cases.
        #[test]
        fn new_encoder_matches_recovered_original(value in 1u128..=u128::MAX) {
            assert_encoders_agree(value);
        }
    }

    /// A value with any high-order (big-endian) byte set exceeds `u128` and is
    /// rejected. `[15]` is the `2^128` position — the first bit above the range.
    #[test]
    fn rejects_chainwork_exceeding_u128() {
        let mut on_disk = [0u8; 32];
        on_disk[15] = 0x01;
        assert!(PersistentChainWork(on_disk).into_business().is_err());
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
