//! Zcash protocol constants (specification facts; restating them elsewhere = bug)

/// No valid reorg rewrites more than this many blocks
pub const MAX_BLOCK_REORG_HEIGHT: u32 = 1000;

/// Serialized block ceiling (protocol spec §7.1 `MAX_BLOCK_SIZE`; a transaction never exceeds it)
pub const MAX_BLOCK_BYTES: usize = 2_000_000;
