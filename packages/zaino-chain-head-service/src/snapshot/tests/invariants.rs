//! [`MapBackedSnapshot`]'s representation invariants, checked against its
//! private fields.
//!
//! These are the invariants only this representation has, plus index entries
//! the read traits cannot see: `best_chain` skips a canonical hash whose
//! block is not retained, so only the index itself shows it.

use std::collections::HashSet;

use zaino_chain_head::ChainHeadSnapshot as _;
use zaino_primitives::types::BlockHash;

use crate::{graph::tests::InspectableGraph, snapshot::MapBackedSnapshot};

impl InspectableGraph for MapBackedSnapshot {
    fn retained_hashes(&self) -> HashSet<BlockHash> {
        self.blocks().map(|block| block.hash()).collect()
    }

    fn retained_block_count(&self) -> usize {
        Self::retained_block_count(self)
    }

    fn check_representation(&self) -> Result<(), String> {
        self.check_tip_not_in_others()?;
        self.check_others_keyed_by_hash()?;
        self.check_canonical_hashes_retained()
    }
}

impl MapBackedSnapshot {
    fn check_tip_not_in_others(&self) -> Result<(), String> {
        if self.others.contains_key(&self.tip.hash()) {
            return Err(format!("others holds the tip {}", self.tip.hash()));
        }
        Ok(())
    }

    fn check_others_keyed_by_hash(&self) -> Result<(), String> {
        match self
            .others
            .iter()
            .find(|(key, block)| **key != block.hash())
        {
            Some((key, block)) => Err(format!(
                "others holds block {} under key {key}",
                block.hash()
            )),
            None => Ok(()),
        }
    }

    /// `∀ (h, x) ∈ heights_to_hashes: x ∈ retained ∧ height(x) = h`
    fn check_canonical_hashes_retained(&self) -> Result<(), String> {
        for (height, hash) in &self.heights_to_hashes {
            let Some(block) = self.block_by_hash(hash) else {
                return Err(format!("canonical {hash} at {height} is not retained"));
            };
            if block.height() != *height {
                return Err(format!(
                    "canonical {hash} is indexed at {height} but sits at {}",
                    block.height()
                ));
            }
        }
        Ok(())
    }
}
