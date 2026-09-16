//! The compact proof-of-work difficulty encoding from the block header.
//!
//! Wraps the 4-byte "nBits" field — a custom floating-point format with an
//! 8-bit exponent and a 24-bit signed mantissa. Many `u32` bit-patterns are
//! invalid (negative target, zero target, overflow); the constructor rejects
//! them so that downstream code can rely on the value being well-formed.
//!
//! Internally delegates to `zebra_chain`'s difficulty types for all
//! arithmetic, but never exposes them in the public API.

use std::fmt;
use std::num::NonZeroU128;

use super::ChainWork;

/// A validated compact difficulty value from a block header.
///
/// Invariant: the inner value always represents a valid, non-negative,
/// non-zero target that can be expanded to a full 256-bit threshold and
/// converted to a work value without error.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct CompactDifficulty(zebra_chain::work::difficulty::CompactDifficulty);

/// Errors that can occur when constructing a [`CompactDifficulty`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompactDifficultyError {
    /// The nBits value does not decode to a valid PoW target (negative,
    /// zero, or overflow according to the Zcash specification §7.7.4).
    #[error("nBits value {bits:#010x} does not encode a valid target: {reason}")]
    InvalidEncoding {
        /// The raw nBits value that failed validation.
        bits: u32,
        /// The upstream validation error message.
        reason: String,
    },
}

impl CompactDifficulty {
    /// Try to build a `CompactDifficulty` from a raw `u32` (native byte order).
    ///
    /// Returns an error if the value cannot be expanded to a valid,
    /// non-negative, non-zero PoW target.
    pub fn try_from_bits(bits: u32) -> Result<Self, CompactDifficultyError> {
        Self::try_from_be_bytes(bits.to_be_bytes())
    }

    /// Try to build a `CompactDifficulty` from big-endian bytes.
    ///
    /// This matches the byte order returned by Zebra's
    /// `bytes_in_display_order()`, avoiding a manual byte-order
    /// conversion at call sites that already have the display bytes.
    pub fn try_from_be_bytes(bytes: [u8; 4]) -> Result<Self, CompactDifficultyError> {
        let bits = u32::from_be_bytes(bytes);
        let zebra =
            zebra_chain::work::difficulty::CompactDifficulty::from_bytes_in_display_order(&bytes)
                .map_err(|e| CompactDifficultyError::InvalidEncoding {
                bits,
                reason: e.to_string(),
            })?;
        Ok(Self(zebra))
    }

    /// Return the raw `u32` nBits value.
    ///
    /// Useful for wire serialization and DB persistence. The returned value
    /// is guaranteed to be a valid compact encoding.
    pub fn as_bits(&self) -> u32 {
        u32::from_be_bytes(self.0.bytes_in_display_order())
    }

    /// Compute the single-block proof-of-work contribution as a [`ChainWork`].
    ///
    /// Walks the full Zebra conversion chain internally:
    /// `CompactDifficulty → ExpandedDifficulty → Work → ChainWork`.
    pub fn to_work(&self) -> ChainWork {
        let work = self
            .0
            .to_work()
            .expect("validated at construction: nBits encodes a valid target");
        // A valid, nonzero target always produces nonzero work.
        ChainWork::new(
            NonZeroU128::new(work.as_u128())
                .expect("valid compact difficulty produces nonzero work"),
        )
    }

    /// Returns a human-readable difficulty as a multiple of the network's
    /// minimum difficulty.
    ///
    /// A result of 1.0 means the block was mined at minimum difficulty.
    pub fn relative_difficulty(&self, network: &zebra_chain::parameters::Network) -> f64 {
        self.0.relative_to_network(network)
    }
}

impl fmt::Debug for CompactDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CompactDifficulty")
            .field(&format_args!("{:#010x}", self.as_bits()))
            .finish()
    }
}

impl fmt::Display for CompactDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid nBits value for use in tests. This value passes zebra's
    /// compact difficulty validation (non-negative, non-zero, no overflow)
    /// but does not correspond to any specific real-world block.
    const TEST_VALID_NBITS: u32 = 0x2007_ffff;

    #[test]
    fn valid_bits_accepted() {
        let cd = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid");
        assert_eq!(cd.as_bits(), TEST_VALID_NBITS);
    }

    #[test]
    fn zero_target_rejected() {
        assert!(CompactDifficulty::try_from_bits(0x0000_0000).is_err());
    }

    #[test]
    fn negative_target_rejected() {
        assert!(CompactDifficulty::try_from_bits(0x0180_0000).is_err());
    }

    #[test]
    fn overflow_rejected() {
        assert!(CompactDifficulty::try_from_bits(u32::MAX).is_err());
    }

    #[test]
    fn round_trip_bits() {
        let cd = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid");
        assert_eq!(cd.as_bits(), TEST_VALID_NBITS);
    }

    #[test]
    fn to_work_is_nonzero() {
        let cd = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid");
        assert!(cd.to_work().as_non_zero_u128().get() > 0);
    }

    /// Full pipeline: on-disk u32 → CompactDifficulty → ChainWork → accumulate.
    ///
    /// Exercises the complete conversion chain that block indexing performs,
    /// without needing real block data.
    #[test]
    fn on_disk_bits_to_accumulated_chainwork() {
        // Simulate two blocks with the same difficulty arriving from disk.
        let bits = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid");
        let block_work = bits.to_work();

        // Genesis: chainwork = block's own work.
        let genesis_chainwork = block_work;
        assert!(genesis_chainwork.as_non_zero_u128().get() > 0);

        // Block 1: parent chainwork + block work.
        let block1_chainwork = genesis_chainwork.add(&block_work).expect("no overflow");
        assert!(block1_chainwork > genesis_chainwork);

        // The accumulated value is exactly 2x the single-block work.
        assert_eq!(
            block1_chainwork.as_non_zero_u128().get(),
            2 * block_work.as_non_zero_u128().get(),
        );
    }
}
