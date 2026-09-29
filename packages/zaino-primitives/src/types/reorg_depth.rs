//! The reorg depth: how far below the tip a block stays reorg-able

use core::num::NonZeroU32;

/// Blocks below the tip kept reorg-able (non-final); consensus bound = 1000
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorgDepth(NonZeroU32);

impl ReorgDepth {
    pub const CONSENSUS: Self =
        Self(NonZeroU32::new(crate::protocol::MAX_BLOCK_REORG_HEIGHT).expect("1000 is non-zero"));

    pub const fn new(depth: NonZeroU32) -> Self {
        Self(depth)
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}
