//! ChainStream — a reader's materialized, thread-safe snapshot cursor.
//!
//! A ChainStream is a snapshot of the chain + a cursor position.
//! Materialization is a forward for-loop — no backward walk, no reverse,
//! no accumulation buffer. O(1) memory regardless of range size.

use crate::chain::Chain;
use crate::error::StoreError;
use crate::types::{Block, Height};

/// A cursor over a height range [start, end], resolving blocks from a
/// captured chain snapshot.
///
/// After the initial snap (one `Arc::clone` under the read lock),
/// iteration is lock-free and allocation-free.
#[derive(Debug, Clone)]
pub struct ChainStream {
    /// Captured chain snapshot.
    chain: Chain,
    /// Below this height, blocks are in LMDB (not covered here).
    freeze_horizon: Height,
    /// Current cursor position.
    current: Height,
    /// Inclusive upper bound.
    end: Height,
}

impl ChainStream {
    /// Create a new ChainStream from a chain snapshot.
    pub(crate) fn new(
        chain: Chain,
        freeze_horizon: Height,
        start: Height,
        end: Height,
    ) -> Self {
        Self {
            chain,
            freeze_horizon,
            current: start,
            end,
        }
    }

    /// Advance the cursor and return the next block.
    ///
    /// Returns `None` when the range is exhausted or resolution fails.
    /// Blocks below `freeze_horizon` return `Err` here — the caller
    /// should fall through to LMDB.
    pub fn next(&mut self) -> Result<Option<&Block>, StoreError> {
        if self.current > self.end {
            return Ok(None);
        }
        if self.current < self.freeze_horizon {
            // Below freeze horizon — caller should use LMDB
            return Err(StoreError::BelowFreezeHorizon(
                self.current,
                self.freeze_horizon,
            ));
        }
        let block = self
            .chain
            .get(self.current)
            .ok_or(StoreError::HeightNotFound(self.current))?;
        self.current += 1;
        Ok(Some(block))
    }

    /// Number of remaining heights in the range.
    pub fn remaining(&self) -> usize {
        if self.current > self.end {
            0
        } else {
            (self.end - self.current + 1) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Chain;
    use crate::types::GENESIS_HASH;

    fn test_block(height: Height, hash: [u8; 32], prev_hash: [u8; 32]) -> Block {
        Block::new(height, hash, prev_hash, vec![height as u8])
    }

    fn test_chain() -> Chain {
        let c = Chain::new(0);
        let b0 = test_block(0, GENESIS_HASH, GENESIS_HASH);
        let b1 = test_block(1, [1u8; 32], GENESIS_HASH);
        let b2 = test_block(2, [2u8; 32], [1u8; 32]);
        c.push_back(b0).push_back(b1).push_back(b2)
    }

    #[test]
    fn chain_stream_iterates_forward() {
        let chain = test_chain();
        let mut stream = ChainStream::new(chain, 0, 0, 2);

        let b0 = stream.next().unwrap().unwrap();
        assert_eq!(b0.height, 0);

        let b1 = stream.next().unwrap().unwrap();
        assert_eq!(b1.height, 1);

        let b2 = stream.next().unwrap().unwrap();
        assert_eq!(b2.height, 2);

        assert!(stream.next().unwrap().is_none());
    }

    #[test]
    fn chain_stream_below_freeze_returns_error() {
        let chain = test_chain();
        // freeze_horizon = 1 means height 0 is on disk
        let mut stream = ChainStream::new(chain, 1, 0, 2);
        assert!(stream.next().is_err());
    }

    #[test]
    fn chain_stream_start_above_zero() {
        // Chain starting at height 5
        let c = Chain::new(5);
        let b5 = test_block(5, [5u8; 32], [4u8; 32]);
        let b6 = test_block(6, [6u8; 32], [5u8; 32]);
        let chain = c.push_back(b5).push_back(b6);

        let mut stream = ChainStream::new(chain, 0, 5, 6);

        let b5 = stream.next().unwrap().unwrap();
        assert_eq!(b5.height, 5);

        let b6 = stream.next().unwrap().unwrap();
        assert_eq!(b6.height, 6);

        assert!(stream.next().unwrap().is_none());
    }
}
