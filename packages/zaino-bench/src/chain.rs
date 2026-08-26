//! Incremental `prev_hash` link verification over a stream of compact blocks.
//!
//! Both the load generator and the serve-rate run need to know whether the
//! blocks they were served actually form a chain — a server that answers fast
//! with garbage is not answering. They differ only in how much detail they
//! report, so the checking itself lives here and each caller reads what it needs.

use zaino_proto::proto::compact_formats::CompactBlock;

use crate::grpc_client::copy_hash;

/// A place where the served blocks stopped forming a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainBreak {
    /// The height at which the break was observed.
    pub(crate) height: u64,
    /// What was wrong, in operator-readable terms.
    pub(crate) detail: String,
}

/// Feed blocks in ascending height order; read the tally when the stream ends.
#[derive(Debug, Default)]
pub(crate) struct ChainVerifier {
    prev_hash: Option<[u8; 32]>,
    last_height: Option<u64>,
    breaks: Vec<ChainBreak>,
    hash_length_errors: usize,
}

impl ChainVerifier {
    /// A verifier with no blocks seen yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Checks `block` against the one before it and records any break.
    pub(crate) fn push(&mut self, block: &CompactBlock) {
        let height = block.height;

        if let Some(last) = self.last_height {
            if height <= last {
                self.record(
                    height,
                    format!("height {height} is not strictly after previous height {last} — blocks out of order"),
                );
            }
        }
        self.last_height = Some(height);

        // A malformed hash is a protocol violation, not a chain break: count it
        // separately and drop the link, so the next block is not also blamed.
        let (Some(hash), Some(prev_hash)) = (copy_hash(&block.hash), copy_hash(&block.prev_hash))
        else {
            self.hash_length_errors += 1;
            self.prev_hash = None;
            return;
        };

        if height == 0 && prev_hash != [0u8; 32] {
            self.record(
                0,
                "genesis block (height 0) must have prevHash = all-zeros".to_string(),
            );
        }

        if let Some(expected) = self.prev_hash {
            if prev_hash != expected {
                self.record(
                    height,
                    format!(
                        "prevHash {} does not match previous block's hash {}",
                        hex(&prev_hash),
                        hex(&expected)
                    ),
                );
            }
        }

        self.prev_hash = Some(hash);
    }

    /// Every break found so far, in the order they were observed.
    pub(crate) fn breaks(&self) -> &[ChainBreak] {
        &self.breaks
    }

    /// How many blocks carried a hash field of the wrong length.
    pub(crate) fn hash_length_errors(&self) -> usize {
        self.hash_length_errors
    }

    /// Breaks plus malformed hashes — the number the summary reports.
    pub(crate) fn total_errors(&self) -> usize {
        self.breaks.len() + self.hash_length_errors
    }

    fn record(&mut self, height: u64, detail: String) {
        self.breaks.push(ChainBreak { height, detail });
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a linked run of `count` blocks starting at `start`, where block
    /// `n`'s hash is `[n; 32]` — enough structure to exercise the link check
    /// without constructing real blocks.
    fn linked(start: u64, count: u64) -> Vec<CompactBlock> {
        (start..start + count)
            .map(|height| CompactBlock {
                height,
                hash: vec![height as u8; 32],
                prev_hash: vec![height.wrapping_sub(1) as u8; 32],
                ..CompactBlock::default()
            })
            .collect()
    }

    fn verify(blocks: &[CompactBlock]) -> ChainVerifier {
        let mut verifier = ChainVerifier::new();
        for block in blocks {
            verifier.push(block);
        }
        verifier
    }

    #[test]
    fn a_linked_run_has_no_errors() {
        let verifier = verify(&linked(100, 10));
        assert_eq!(verifier.total_errors(), 0);
    }

    #[test]
    fn a_broken_link_is_one_break() {
        let mut blocks = linked(100, 10);
        blocks[5].prev_hash = vec![0xaa; 32];

        let verifier = verify(&blocks);
        assert_eq!(verifier.breaks().len(), 1);
        assert_eq!(verifier.breaks()[0].height, 105);
        assert_eq!(verifier.hash_length_errors(), 0);
    }

    #[test]
    fn a_short_hash_is_a_length_error_not_a_break() {
        let mut blocks = linked(100, 10);
        blocks[5].hash = vec![0xaa; 31];

        let verifier = verify(&blocks);
        assert_eq!(verifier.hash_length_errors(), 1);
        // The block after the malformed one has no link to check against, so it
        // is not blamed for the gap.
        assert_eq!(verifier.breaks(), &[]);
    }

    #[test]
    fn genesis_must_have_a_zero_prev_hash() {
        let block = CompactBlock {
            height: 0,
            hash: vec![1u8; 32],
            prev_hash: vec![9u8; 32],
            ..CompactBlock::default()
        };

        let verifier = verify(&[block]);
        assert_eq!(verifier.breaks().len(), 1);
        assert_eq!(verifier.breaks()[0].height, 0);
    }

    #[test]
    fn genesis_with_a_zero_prev_hash_is_clean() {
        let block = CompactBlock {
            height: 0,
            hash: vec![1u8; 32],
            prev_hash: vec![0u8; 32],
            ..CompactBlock::default()
        };

        assert_eq!(verify(&[block]).total_errors(), 0);
    }

    #[test]
    fn out_of_order_heights_are_a_break() {
        let mut blocks = linked(100, 3);
        blocks.swap(1, 2);

        let verifier = verify(&blocks);
        assert!(!verifier.breaks().is_empty());
    }
}
