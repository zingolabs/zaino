//! Aliases for fields not yet promoted to newtypes (promote on first misuse)

/// Output index within a transaction
pub type OutputIndex = u32;

/// Unix epoch seconds
pub type BlockTime = u32;

pub type EquihashNonce = [u8; 32];
