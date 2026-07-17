//! Descending sample of a driver's view of the chain.

use zaino_primitives::types::BlockHash;

use crate::block_id::BlockId;

/// A descending-by-height sample of blocks a driver believes are on the
/// chain, used to detect the fork point between the driver's view and
/// the best chain.
///
/// The invariant — at least one entry, heights strictly descending — is
/// enforced at construction, so every capability that accepts a locator
/// can rely on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLocator(Vec<BlockId>);

/// Error returned when a locator's entries violate the invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlockLocatorError {
    /// A locator must contain at least one entry.
    #[error("a block locator must contain at least one entry")]
    Empty,

    /// Heights must strictly descend from the first entry to the last.
    #[error("locator heights must strictly descend: entry {position} does not")]
    NotDescending {
        /// Index of the first entry whose height fails to descend.
        position: usize,
    },
}

impl BlockLocator {
    /// Validate and wrap locator entries.
    pub fn new(entries: Vec<BlockId>) -> Result<Self, BlockLocatorError> {
        if entries.is_empty() {
            return Err(BlockLocatorError::Empty);
        }
        for (position, pair) in entries.windows(2).enumerate() {
            if pair[1].height >= pair[0].height {
                return Err(BlockLocatorError::NotDescending {
                    position: position + 1,
                });
            }
        }
        Ok(Self(entries))
    }

    /// The entries, highest first.
    pub fn entries(&self) -> &[BlockId] {
        &self.0
    }

    /// The entry hashes, highest first.
    pub fn hashes(&self) -> impl Iterator<Item = BlockHash> + '_ {
        self.0.iter().map(|entry| entry.hash)
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::Height;

    use super::*;

    fn entry(height: u32, byte: u8) -> BlockId {
        BlockId {
            height: Height::try_from(height).expect("valid height"),
            hash: BlockHash::from([byte; 32]),
        }
    }

    #[test]
    fn descending_entries_accepted() {
        let locator =
            BlockLocator::new(vec![entry(100, 1), entry(90, 2), entry(0, 3)]).expect("valid");
        assert_eq!(locator.entries().len(), 3);
    }

    #[test]
    fn single_entry_accepted() {
        assert!(BlockLocator::new(vec![entry(0, 1)]).is_ok());
    }

    #[test]
    fn empty_rejected() {
        assert_eq!(BlockLocator::new(vec![]), Err(BlockLocatorError::Empty));
    }

    #[test]
    fn ascending_rejected_at_position() {
        let err = BlockLocator::new(vec![entry(90, 1), entry(100, 2)]).unwrap_err();
        assert_eq!(err, BlockLocatorError::NotDescending { position: 1 });
    }

    #[test]
    fn equal_heights_rejected() {
        let err = BlockLocator::new(vec![entry(90, 1), entry(90, 2)]).unwrap_err();
        assert_eq!(err, BlockLocatorError::NotDescending { position: 1 });
    }

    #[test]
    fn hashes_preserve_order() {
        let locator = BlockLocator::new(vec![entry(100, 1), entry(90, 2)]).expect("valid");
        let hashes: Vec<BlockHash> = locator.hashes().collect();
        assert_eq!(
            hashes,
            vec![BlockHash::from([1u8; 32]), BlockHash::from([2u8; 32])]
        );
    }
}
