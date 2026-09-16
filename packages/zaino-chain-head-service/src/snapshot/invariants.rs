//! The invariants [`MapBackedSnapshot`] documents, checked against its private
//! fields.
//!
//! Test-only. A property test runs this after every move, so a move that
//! breaks an invariant fails at the step that broke it.

use std::collections::HashSet;

use zaino_chain_head::ChainHeadSnapshot as _;
use zaino_primitives::types::{BlockHash, Height};

use super::MapBackedSnapshot;

impl MapBackedSnapshot {
    /// Every retained block's hash, the tip included.
    pub(crate) fn retained_hashes(&self) -> HashSet<BlockHash> {
        self.blocks().map(|block| block.hash()).collect()
    }

    /// The first documented invariant that does not hold, if any.
    pub(crate) fn check_invariants(&self) -> Result<(), String> {
        self.check_tip_not_in_others()?;
        self.check_others_keyed_by_hash()?;
        self.check_canonical_hashes_retained()?;
        self.check_canonical_chain_ends_at_tip()?;
        self.check_canonical_chain_is_linked()
    }

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

    /// `max(dom heights_to_hashes) = height(tip) ∧ heights_to_hashes(height(tip)) = tip`
    fn check_canonical_chain_ends_at_tip(&self) -> Result<(), String> {
        let top = self.heights_to_hashes.keys().max();
        if top != Some(&self.tip.height()) {
            return Err(format!(
                "highest canonical height {top:?} is not the tip's {}",
                self.tip.height()
            ));
        }
        if self.heights_to_hashes.get(&self.tip.height()) != Some(&self.tip.hash()) {
            return Err(format!(
                "the tip {} is not canonical at its height",
                self.tip.hash()
            ));
        }
        Ok(())
    }

    /// Heights run without gaps from the lowest to the tip, and each canonical
    /// block's parent is the canonical block one height below.
    fn check_canonical_chain_is_linked(&self) -> Result<(), String> {
        let mut heights: Vec<Height> = self.heights_to_hashes.keys().copied().collect();
        heights.sort_unstable();
        for pair in heights.windows(2) {
            let [below, above] = pair else {
                continue;
            };
            if below.checked_add(1) != Some(*above) {
                return Err(format!("canonical heights skip from {below} to {above}"));
            }
            let parent = self
                .best_block_by_height(*above)
                .map(|block| block.parent_hash);
            let expected = self.heights_to_hashes.get(below).copied();
            if parent != expected {
                return Err(format!(
                    "canonical block at {above} has parent {parent:?}, not {expected:?}"
                ));
            }
        }
        Ok(())
    }
}
