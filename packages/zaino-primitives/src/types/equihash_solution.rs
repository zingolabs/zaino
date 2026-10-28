//! Equihash solution as it appears in a block header.

/// The proof-of-work solution carried in a block header.
///
/// Zaino does not validate proof of work, so nothing here inspects the
/// solution. It is carried because it is part of the header: a consumer that
/// re-serializes a block, or persists one and later reconstructs it, needs the
/// bytes the block hash actually commits to. Dropping it would make the domain
/// block a lossy projection of the consensus block.
///
/// The two variants are the two Equihash parameterisations Zcash uses; the
/// length is what distinguishes them on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// The `Standard` variant is 1344 bytes and dominates the enum's size. Boxing it
// would move a per-block allocation onto the heap for no gain: a header is
// built once per block and read as bytes.
#[allow(clippy::large_enum_variant)]
pub enum EquihashSolution {
    /// 200-9 solution (mainnet / testnet).
    Standard([u8; 1344]),
    /// 48-5 solution (regtest).
    Regtest([u8; 36]),
}

impl EquihashSolution {
    /// The solution's bytes, without the length prefix its wire encoding
    /// carries.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Standard(bytes) => bytes,
            Self::Regtest(bytes) => bytes,
        }
    }
}
