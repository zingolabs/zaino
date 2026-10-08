//! Commitment treestate over the non-finalised window.
//!
//! The window retains only the recent chain, so it holds no commitment tree of
//! its own: it answers a treestate by folding its blocks forward from the
//! finalised frontier at the watermark — the `seed` the composer reads from the
//! store and passes in. The fold reuses the `tree_state` index's own per-block
//! extraction (via [`fold_window`]), so a value served here is byte-identical to
//! the one the store will hold once those blocks finalise, and the two sides of
//! the seam agree.
//!
//! That is why this tier implements the narrower
//! [`TreestateWindowRead`] rather than `TreestateRead`: it cannot seed itself.
//! On a reorg the window simply refolds from the same seed over its new best
//! chain, so a treestate served after a reorg reflects the new branch.
//!
//! In-window batches are small, so the serial fold is fine. This also supplies
//! the window's own tree sizes, which closes the NFS `tree_size = 0` seam gap for
//! the treestate read specifically (a compact-block read still projects sizes
//! from each block's carried `tree_roots`).

use zaino_chain_head::ChainHeadSnapshot;
use zaino_indexes::indexes::tree_state::serve::{
    fold_window, seed_value, window_subtree_roots, WindowSubtree,
};
use zaino_indexes::sets::current_zaino::tree_state_ctx;
use zaino_primitives::types::{
    Height, PoolActivations, ShieldedPool, SubtreeRoot, TreeRoot, Treestate,
};
use zaino_service::error::TreestateReadError;
use zaino_service::TreestateWindowRead;

use crate::serve::HeadSnapshot;

impl TreestateWindowRead for HeadSnapshot {
    async fn window_treestate(
        &self,
        seed: Option<&Treestate>,
        activations: PoolActivations,
        at: Height,
    ) -> Result<Option<Treestate>, TreestateReadError> {
        let window = self.window();
        if at > window.best_tip().height {
            // No block at `at`: a domain miss, not a failure.
            return Ok(None);
        }

        // The seam: the window folds the blocks strictly above the seed height
        // (or from genesis when the store holds nothing). Its blocks must be
        // contiguous from there up to `at`, or the window does not reach the
        // seam (the initial-build gap) and cannot fold a correct frontier.
        let seam = seam_next(seed);
        let mut ctxs = Vec::new();
        let mut expected = seam;
        let mut at_identity = None;
        for block in window.best_chain() {
            let height = u32::from(block.height());
            if height < seam {
                continue;
            }
            if height > u32::from(at) {
                break;
            }
            if height != expected {
                // A hole below `at`: the window does not reach the seam.
                return Err(TreestateReadError::NotServiceable(
                    zaino_service::Capability::Treestate,
                ));
            }
            ctxs.push(tree_state_ctx(&block.block));
            if height == u32::from(at) {
                at_identity = Some((block.reference.hash, block.block.header.time));
            }
            expected = expected
                .checked_add(1)
                .ok_or_else(|| TreestateReadError::Fatal("window height overflow".to_owned()))?;
        }
        // The seam is reached exactly when the folded run ended one past `at`. A
        // protocol height is below `u32::MAX`, so the `+1` cannot overflow; name
        // that invariant rather than asserting a magnitude.
        let one_past_at = u32::from(at)
            .checked_add(1)
            .expect("a protocol height is below u32::MAX, so one past it fits u32");
        if expected != one_past_at {
            // `at` was not reached from the seam — a gap, or `at` below the floor.
            return Err(TreestateReadError::NotServiceable(
                zaino_service::Capability::Treestate,
            ));
        }
        let (block_hash, time) = at_identity.ok_or_else(|| {
            TreestateReadError::Fatal(
                "window covered `at` but produced no block identity".to_owned(),
            )
        })?;

        let seed = seed_value(seed)
            .map_err(|e| TreestateReadError::Fatal(format!("decode seed frontier: {e}")))?;
        let folded = fold_window(&seed, &ctxs)
            .map_err(|e| TreestateReadError::Fatal(format!("fold window treestate: {e}")))?;
        let (sapling, orchard, ironwood) = folded.pool_treestates(&activations, at);
        Ok(Some(Treestate {
            block_hash,
            height: at,
            time,
            sapling,
            orchard,
            ironwood,
        }))
    }

    async fn window_subtree_roots(
        &self,
        seed: Option<&Treestate>,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        let window = self.window();
        let seam = seam_next(seed);
        // The whole window above the seam contributes completions.
        let ctxs: Vec<_> = window
            .best_chain()
            .filter(|block| u32::from(block.height()) >= seam)
            .map(|block| tree_state_ctx(&block.block))
            .collect();

        let seed = seed_value(seed)
            .map_err(|e| TreestateReadError::Fatal(format!("decode seed frontier: {e}")))?;
        let completions = window_subtree_roots(&seed, &ctxs, pool)
            .map_err(|e| TreestateReadError::Fatal(format!("fold window subtree roots: {e}")))?;

        let start = u32::from(start_index);
        let end = limit.map(|limit| start.saturating_add(u32::from(limit)));
        completions
            .into_iter()
            .filter(|completion| {
                completion.index >= start && end.is_none_or(|end| completion.index < end)
            })
            .map(domain_subtree_root)
            .collect()
    }
}

/// The first height above the seed the window folds from: one past the watermark,
/// or genesis when the store holds nothing. A protocol height is below
/// `u32::MAX`, so one past it cannot overflow — named rather than asserted as a
/// magnitude, consistent with the fold loop's checked step.
fn seam_next(seed: Option<&Treestate>) -> u32 {
    seed.map_or(0, |seed| {
        u32::from(seed.height)
            .checked_add(1)
            .expect("a protocol height is below u32::MAX, so one past it fits u32")
    })
}

/// A window completion as a domain [`SubtreeRoot`], converting the completing
/// height (fatal if it ever exceeds the protocol ceiling, which a real block
/// cannot).
fn domain_subtree_root(completion: WindowSubtree) -> Result<SubtreeRoot, TreestateReadError> {
    let end_height = u32::try_from(completion.completing_height.value())
        .ok()
        .and_then(|height| Height::try_from(height).ok())
        .ok_or_else(|| {
            TreestateReadError::Fatal(
                "a window subtree completing height exceeds the protocol limit".to_owned(),
            )
        })?;
    Ok(SubtreeRoot {
        root: TreeRoot::from(completion.root),
        end_height,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_chain_head::{ChainHeadBlock, ChainHeadWork};
    use zaino_primitives::types::{
        Block, BlockCommitments, BlockHash, BlockHeader, BlockRef, ChainMetadata,
        CompactCiphertext, CompactDifficulty, EphemeralKey, EquihashSolution, Height, MerkleRoot,
        NoteCommitment, OrchardData, PoolActivations, SaplingData, SaplingOutput, Transaction,
        TransparentData, TreeRoots, Treestate,
    };
    use zaino_service::TreestateWindowRead;

    use crate::graph::ChainGraph;
    use crate::serve::HeadSnapshot;
    use crate::snapshot::MapBackedSnapshot;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("a valid test height")
    }

    /// Sapling active from genesis, so the window's test blocks report it.
    fn active() -> PoolActivations {
        PoolActivations {
            sapling: Some(height(0)),
            orchard: None,
            ironwood: None,
        }
    }

    /// A canonical Sapling note commitment from a seed in its low 8 bytes.
    fn cmu(seed: u64) -> NoteCommitment {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        NoteCommitment::from(bytes)
    }

    /// A transaction paying `cmus` Sapling outputs.
    fn sapling_tx(txid_byte: u8, cmus: &[u64]) -> Transaction {
        Transaction {
            txid: zaino_primitives::types::TransactionId::from([txid_byte; 32]),
            transparent: TransparentData::default(),
            sapling: SaplingData {
                outputs: cmus
                    .iter()
                    .map(|seed| SaplingOutput {
                        cmu: cmu(*seed),
                        ephemeral_key: EphemeralKey::from([0u8; 32]),
                        enc_ciphertext: CompactCiphertext::from([0u8; 52]),
                    })
                    .collect(),
                ..SaplingData::default()
            },
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        }
    }

    fn block(h: u32, hash_byte: u8, parent_byte: u8, txs: Vec<Transaction>) -> ChainHeadBlock {
        let hash = BlockHash::from([hash_byte; 32]);
        let parent_hash = BlockHash::from([parent_byte; 32]);
        ChainHeadBlock {
            reference: BlockRef {
                hash,
                height: height(h),
            },
            parent_hash,
            work: ChainHeadWork::anchored_at(u128::from(h)),
            block: Block {
                header: BlockHeader {
                    hash,
                    version: 4,
                    prev_hash: parent_hash,
                    height: height(h),
                    time: 1_000 + h,
                    merkle_root: MerkleRoot::from([0; 32]),
                    block_commitments: BlockCommitments::from([0; 32]),
                    bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                    nonce: [0; 32],
                    solution: EquihashSolution::Regtest([0; 36]),
                },
                transactions: txs,
                chain_metadata: ChainMetadata::ZERO,
            },
            tree_roots: TreeRoots {
                sapling: None,
                orchard: None,
                ironwood: None,
            },
        }
    }

    /// Review Focus #3: after a reorg, a treestate served from the window
    /// reflects the new branch. The window folds from genesis (no seed); height 1
    /// first mines Sapling note A, then a heavier competing block mines note B, so
    /// the treestate at height 1 changes to the new branch's.
    #[tokio::test]
    async fn window_treestate_follows_a_reorg() {
        let mut graph = MapBackedSnapshot::from_initial_block(block(0, 0x00, 0xFF, vec![]));
        graph
            .extend(block(1, 0x1A, 0x00, vec![sapling_tx(0xA1, &[1])]))
            .expect("1a extends the tip");
        let before = HeadSnapshot::over(Arc::new(graph.clone()));
        let branch_a = before
            .window_treestate(None, active(), height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");

        // Reorg: rewind to genesis, then mine a competing height 1 with a
        // different Sapling note.
        graph
            .rewind_to(BlockRef {
                hash: BlockHash::from([0x00; 32]),
                height: height(0),
            })
            .expect("genesis is on the best chain");
        graph
            .extend(block(1, 0x1B, 0x00, vec![sapling_tx(0xB1, &[2])]))
            .expect("1b extends genesis");
        let after = HeadSnapshot::over(Arc::new(graph));
        let branch_b = after
            .window_treestate(None, active(), height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");

        assert_eq!(branch_a.block_hash, BlockHash::from([0x1A; 32]));
        assert_eq!(branch_b.block_hash, BlockHash::from([0x1B; 32]));
        let a = branch_a.sapling.expect("branch A funded Sapling");
        let b = branch_b.sapling.expect("branch B funded Sapling");
        assert_ne!(
            a.final_root, b.final_root,
            "a different note on the new branch yields a different Sapling root"
        );
        assert_ne!(
            a.final_state, b.final_state,
            "and a different serialized tree"
        );
    }

    /// Step 1(c), the seam: a height just above the watermark, folded by the
    /// window from the finalised seed, equals the same height folded over the
    /// whole chain from genesis. Folding from the finalised prefix (what the
    /// store hands over at the watermark) is indistinguishable from never having
    /// split the chain — so the store at the watermark and the window just above
    /// it agree.
    #[tokio::test]
    async fn window_agrees_with_the_finalised_seed_at_the_seam() {
        use zaino_indexes::indexes::tree_state::serve::{fold_window, seed_value};
        use zaino_indexes::sets::current_zaino::tree_state_ctx;

        let b0 = block(0, 0x00, 0xFF, vec![sapling_tx(0xA0, &[10, 11])]);
        let b1 = block(1, 0x01, 0x00, vec![sapling_tx(0xA1, &[12])]);
        let b2 = block(2, 0x02, 0x01, vec![sapling_tx(0xA2, &[13, 14])]);

        // The finalised value at the watermark (height 1), as the store would
        // hold it: fold genesis..=1 through the index's own extraction.
        let empty = seed_value(None).expect("empty seed");
        let value_at_1 = fold_window(
            &empty,
            &[tree_state_ctx(&b0.block), tree_state_ctx(&b1.block)],
        )
        .expect("fold to height 1");
        let (sapling, orchard, ironwood) = value_at_1.pool_treestates(&active(), height(1));
        let seed = Treestate {
            block_hash: b1.reference.hash,
            height: height(1),
            time: b1.block.header.time,
            sapling,
            orchard,
            ironwood,
        };

        // Window A: floor at height 1 (the finalised prefix is elsewhere), folds
        // height 2 from the seed.
        let mut floor1 = MapBackedSnapshot::from_initial_block(b1.clone());
        floor1.extend(b2.clone()).expect("2 extends 1");
        let seeded = HeadSnapshot::over(Arc::new(floor1))
            .window_treestate(Some(&seed), active(), height(2))
            .await
            .expect("read succeeds")
            .expect("height 2 is in the window");

        // Window B: the whole chain from genesis, no seed, folds height 2.
        let mut floor0 = MapBackedSnapshot::from_initial_block(b0.clone());
        floor0.extend(b1).expect("1 extends 0");
        floor0.extend(b2).expect("2 extends 1");
        let whole = HeadSnapshot::over(Arc::new(floor0))
            .window_treestate(None, active(), height(2))
            .await
            .expect("read succeeds")
            .expect("height 2 is in the window");

        assert_eq!(
            seeded, whole,
            "folding from the finalised seed at the watermark equals folding the whole chain"
        );
    }

    /// A height above the window tip is a domain miss, not a failure.
    #[tokio::test]
    async fn window_treestate_above_the_tip_is_a_miss() {
        let graph = MapBackedSnapshot::from_initial_block(block(0, 0x00, 0xFF, vec![]));
        let head = HeadSnapshot::over(Arc::new(graph));
        assert_eq!(
            head.window_treestate(None, active(), height(9))
                .await
                .expect("read succeeds"),
            None,
        );
    }
}
