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
//! On a reorg the window folds its new best chain from the same seed, so a
//! treestate served after a reorg reflects the new branch.
//!
//! A full window is about a thousand blocks, and folding one block hashes every
//! note commitment it adds, so the fold is not repeated per read. Each block's
//! result is cached by hash in [`WindowFrontiers`], shared across snapshots: a
//! read folds only the blocks no earlier read has, and does that on the blocking
//! pool. This also supplies the window's own tree sizes, which closes the NFS
//! `tree_size = 0` seam gap for the treestate read specifically (a compact-block
//! read still projects sizes from each block's carried `tree_roots`).

mod frontier_cache;

pub(crate) use frontier_cache::WindowFrontiers;

use std::sync::Arc;

use zaino_async::{run_blocking, TaskError, TaskName};
use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
use zaino_indexes::indexes::tree_state::serve::{
    fold_window, seed_value, window_subtree_roots, WindowSubtree,
};
use zaino_indexes::indexes::tree_state::TreeStateValue;
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
        if at > self.window().best_tip().height {
            // No block at `at`: a domain miss, not a failure.
            return Ok(None);
        }
        let run = seam_run(self, seed, Some(at))?;
        let Some(last) = run.last() else {
            // `at` is at or below the watermark: not this tier's height.
            return Err(not_serviceable());
        };
        let (block_hash, time) = (last.reference.hash, last.block.header.time);
        let after = self.frontiers_after(seed, &run).await?;
        let folded = after.last().ok_or_else(|| {
            TreestateReadError::Fatal("a non-empty window run folded to no value".to_owned())
        })?;
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
        // The whole window above the seam contributes completions.
        let run = seam_run(self, seed, None)?;
        let completions = self.completions(seed, &run, pool).await?;

        let start = u32::from(start_index);
        let end = limit.map(|limit| start.saturating_add(u32::from(limit)));
        completions
            .iter()
            .flat_map(|block| block.iter())
            .filter(|completion| {
                completion.index >= start && end.is_none_or(|end| completion.index < end)
            })
            .cloned()
            .map(domain_subtree_root)
            .collect()
    }
}

impl HeadSnapshot {
    /// The frontier after each block of `run`, in order.
    ///
    /// Served from the cache where every block is already folded. Otherwise the
    /// read takes the fill lock, rechecks (a concurrent read may have folded the
    /// same blocks meanwhile), and folds from the first block with no cached
    /// frontier on the blocking pool, starting from its parent's cached frontier
    /// or, for the first block above the seam, from the seed.
    async fn frontiers_after(
        &self,
        seed: Option<&Treestate>,
        run: &[&ChainHeadBlock],
    ) -> Result<Vec<Arc<TreeStateValue>>, TreestateReadError> {
        let cache = self.frontiers();
        if let Some(seed) = seed {
            cache.prune_through(seed.height);
        }
        let mut after: Vec<Option<Arc<TreeStateValue>>> =
            run.iter().map(|block| cache.after(&block.hash())).collect();
        if after.iter().all(Option::is_some) {
            return Ok(after.into_iter().flatten().collect());
        }

        let _fill = cache.fill_guard().await;
        for (slot, block) in after.iter_mut().zip(run) {
            if slot.is_none() {
                *slot = cache.after(&block.hash());
            }
        }
        let Some(first_missing) = after.iter().position(Option::is_none) else {
            return Ok(after.into_iter().flatten().collect());
        };
        let base =
            match first_missing.checked_sub(1) {
                None => Arc::new(decode_seed(seed)?),
                Some(parent) => after.get(parent).cloned().flatten().ok_or_else(|| {
                    fatal("a block before the first unfolded one has no frontier")
                })?,
            };

        let todo: Vec<_> = run
            .get(first_missing..)
            .unwrap_or_default()
            .iter()
            .map(|block| (block.hash(), block.height(), tree_state_ctx(&block.block)))
            .collect();
        let folded = run_blocking(TaskName("window-treestate-fold"), move || {
            let mut carry = base;
            let mut folded = Vec::with_capacity(todo.len());
            for (hash, height, ctx) in todo {
                let next = Arc::new(fold_window(&carry, std::slice::from_ref(&ctx))?);
                folded.push((hash, height, Arc::clone(&next)));
                carry = next;
            }
            Ok(folded)
        })
        .await
        .map_err(task_error)?
        .map_err(
            |e: zaino_indexes::indexes::tree_state::sync::TreeStateError| {
                fatal(format!("fold window treestate: {e}"))
            },
        )?;

        for ((hash, height, value), slot) in
            folded.into_iter().zip(after.iter_mut().skip(first_missing))
        {
            cache.insert_after(hash, height, Arc::clone(&value));
            *slot = Some(value);
        }
        after
            .into_iter()
            .map(|value| value.ok_or_else(|| fatal("a window block was left unfolded")))
            .collect()
    }

    /// The subtrees each block of `run` completes in `pool`, in order.
    ///
    /// A block's completions are computed once, on top of its parent's
    /// frontier, and cached with it.
    async fn completions(
        &self,
        seed: Option<&Treestate>,
        run: &[&ChainHeadBlock],
        pool: ShieldedPool,
    ) -> Result<Vec<Arc<[WindowSubtree]>>, TreestateReadError> {
        let cache = self.frontiers();
        let mut completions: Vec<Option<Arc<[WindowSubtree]>>> = run
            .iter()
            .map(|block| cache.completions(&block.hash(), pool))
            .collect();
        if completions.iter().all(Option::is_some) {
            return Ok(completions.into_iter().flatten().collect());
        }

        let after = self.frontiers_after(seed, run).await?;
        let _fill = cache.fill_guard().await;
        let mut todo = Vec::new();
        for (index, (slot, block)) in completions.iter_mut().zip(run).enumerate() {
            if slot.is_some() {
                continue;
            }
            *slot = cache.completions(&block.hash(), pool);
            if slot.is_some() {
                continue;
            }
            let parent = match index.checked_sub(1) {
                None => Arc::new(decode_seed(seed)?),
                Some(parent) => after
                    .get(parent)
                    .cloned()
                    .ok_or_else(|| fatal("a window block's parent has no frontier"))?,
            };
            todo.push((index, block.hash(), parent, tree_state_ctx(&block.block)));
        }

        let computed = run_blocking(TaskName("window-subtree-roots"), move || {
            todo.into_iter()
                .map(|(index, hash, parent, ctx)| {
                    window_subtree_roots(&parent, std::slice::from_ref(&ctx), pool)
                        .map(|found| (index, hash, Arc::<[WindowSubtree]>::from(found)))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(task_error)?
        .map_err(|e| fatal(format!("fold window subtree roots: {e}")))?;

        for (index, hash, found) in computed {
            cache.insert_completions(&hash, pool, Arc::clone(&found));
            if let Some(slot) = completions.get_mut(index) {
                *slot = Some(found);
            }
        }
        completions
            .into_iter()
            .map(|found| found.ok_or_else(|| fatal("a window block was left without completions")))
            .collect()
    }
}

/// The window's best-chain blocks from just above the seam up to `upto` (the
/// whole window when `None`), in ascending height.
///
/// The run must be contiguous from the seam: a hole means the window does not
/// reach the seam (the initial-build gap) and cannot fold a correct frontier.
/// With `upto`, the run must also end exactly there.
fn seam_run<'a>(
    head: &'a HeadSnapshot,
    seed: Option<&Treestate>,
    upto: Option<Height>,
) -> Result<Vec<&'a ChainHeadBlock>, TreestateReadError> {
    let seam = seam_next(seed);
    let mut run = Vec::new();
    let mut expected = seam;
    for block in head.window().best_chain() {
        let height = u32::from(block.height());
        if height < seam {
            continue;
        }
        if upto.is_some_and(|upto| height > u32::from(upto)) {
            break;
        }
        if height != expected {
            return Err(not_serviceable());
        }
        run.push(block);
        expected = expected
            .checked_add(1)
            .ok_or_else(|| fatal("window height overflow"))?;
    }
    if let Some(upto) = upto {
        // `upto` was reached exactly when the run ended at it; a run that stops
        // short, or an `upto` below the seam, is not this tier's to answer.
        if run.last().is_some_and(|block| block.height() != upto) {
            return Err(not_serviceable());
        }
    }
    Ok(run)
}

/// The finalised frontier at the watermark, decoded from the seed treestate.
fn decode_seed(seed: Option<&Treestate>) -> Result<TreeStateValue, TreestateReadError> {
    seed_value(seed).map_err(|e| fatal(format!("decode seed frontier: {e}")))
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

fn not_serviceable() -> TreestateReadError {
    TreestateReadError::NotServiceable(zaino_service::Capability::Treestate)
}

fn fatal(message: impl Into<String>) -> TreestateReadError {
    TreestateReadError::Fatal(message.into())
}

/// Fold a blocking-pool failure into this read's error: a panic in the fold is a
/// defect (fatal); a task dropped on runtime shutdown says nothing about the
/// read (transient).
fn task_error(error: TaskError) -> TreestateReadError {
    match error {
        TaskError::Panicked { .. } => fatal(error.to_string()),
        TaskError::Cancelled { .. } => TreestateReadError::Transient(error.to_string()),
    }
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
        completing_block_hash: completion.completing_block_hash,
        end_height,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot, ChainHeadWork};
    use zaino_primitives::types::{
        Block, BlockCommitments, BlockHash, BlockHeader, BlockRef, ChainMetadata,
        CompactCiphertext, CompactDifficulty, EphemeralKey, EquihashSolution, Height, MerkleRoot,
        NoteCommitment, OrchardData, PoolActivations, SaplingData, SaplingOutput, ShieldedPool,
        Transaction, TransparentData, TreeRoots, Treestate,
    };
    use zaino_service::TreestateWindowRead;

    use crate::graph::ChainGraph;
    use crate::serve::HeadSnapshot;
    use crate::snapshot::MapBackedSnapshot;
    use crate::tree_state::WindowFrontiers;

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

    /// A chain of `len` blocks from genesis, each mining one Sapling note.
    fn funded_chain(len: u8) -> MapBackedSnapshot {
        let mut graph = MapBackedSnapshot::from_initial_block(block(
            0,
            0x00,
            0xFF,
            vec![sapling_tx(0xA0, &[100])],
        ));
        for h in 1..len {
            graph
                .extend(block(
                    u32::from(h),
                    h,
                    h - 1,
                    vec![sapling_tx(0xA0 + h, &[100 + u64::from(h)])],
                ))
                .expect("each block extends the tip");
        }
        graph
    }

    /// A read through a warm cache answers what a cold fold answers, and folds
    /// only the block published since: the next snapshot shares the cache and
    /// adds one entry.
    #[tokio::test]
    async fn a_cached_fold_matches_a_cold_one_and_extends_by_the_new_block() {
        let frontiers = Arc::new(WindowFrontiers::default());
        let mut graph = funded_chain(5);
        let first = HeadSnapshot::new(Arc::new(graph.clone()), Arc::clone(&frontiers));
        let warm_first = first
            .window_treestate(None, active(), height(4))
            .await
            .expect("read succeeds");
        assert_eq!(
            frontiers.len(),
            5,
            "every block up to the asked height is folded"
        );
        let warm_again = first
            .window_treestate(None, active(), height(2))
            .await
            .expect("read succeeds");
        assert_eq!(
            frontiers.len(),
            5,
            "a lower height is answered from the cache"
        );

        graph
            .extend(block(5, 5, 4, vec![sapling_tx(0xA5, &[105])]))
            .expect("5 extends the tip");
        let next = HeadSnapshot::new(Arc::new(graph.clone()), Arc::clone(&frontiers));
        let warm_next = next
            .window_treestate(None, active(), height(5))
            .await
            .expect("read succeeds");
        assert_eq!(
            frontiers.len(),
            6,
            "the next snapshot folds only its new block"
        );

        let cold = HeadSnapshot::over(Arc::new(graph));
        for (warm, at) in [(warm_first, 4), (warm_again, 2), (warm_next, 5)] {
            let cold = cold
                .window_treestate(None, active(), height(at))
                .await
                .expect("read succeeds");
            assert_eq!(warm, cold, "height {at}: cached and cold folds agree");
        }
    }

    /// The cache is keyed by block, not height: after a reorg the competing
    /// block at the same height is folded on its own, through the same cache.
    #[tokio::test]
    async fn a_shared_cache_follows_a_reorg() {
        let frontiers = Arc::new(WindowFrontiers::default());
        let mut graph = MapBackedSnapshot::from_initial_block(block(0, 0x00, 0xFF, vec![]));
        graph
            .extend(block(1, 0x1A, 0x00, vec![sapling_tx(0xA1, &[1])]))
            .expect("1a extends the tip");
        let branch_a = HeadSnapshot::new(Arc::new(graph.clone()), Arc::clone(&frontiers))
            .window_treestate(None, active(), height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");

        graph
            .rewind_to(BlockRef {
                hash: BlockHash::from([0x00; 32]),
                height: height(0),
            })
            .expect("genesis is on the best chain");
        graph
            .extend(block(1, 0x1B, 0x00, vec![sapling_tx(0xB1, &[2])]))
            .expect("1b extends genesis");
        let after = Arc::new(graph);
        let branch_b = HeadSnapshot::new(Arc::clone(&after), Arc::clone(&frontiers))
            .window_treestate(None, active(), height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");
        let cold_b = HeadSnapshot::over(after)
            .window_treestate(None, active(), height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");

        assert_ne!(
            branch_a, branch_b,
            "the new branch is not served the old fold"
        );
        assert_eq!(
            branch_b, cold_b,
            "the new branch's fold matches a cold fold"
        );
    }

    /// A seed at height `w` drops every cached block at or below `w`.
    #[tokio::test]
    async fn the_watermark_prunes_the_cache() {
        use zaino_indexes::indexes::tree_state::serve::{fold_window, seed_value};
        use zaino_indexes::sets::current_zaino::tree_state_ctx;

        let frontiers = Arc::new(WindowFrontiers::default());
        let graph = funded_chain(4);
        let head = HeadSnapshot::new(Arc::new(graph.clone()), Arc::clone(&frontiers));
        head.window_treestate(None, active(), height(3))
            .await
            .expect("read succeeds");
        assert_eq!(frontiers.len(), 4);

        // The store's value at height 1, as the composer would hand it over.
        let ctxs: Vec<_> = graph
            .best_chain()
            .take(2)
            .map(|block| tree_state_ctx(&block.block))
            .collect();
        let value_at_1 =
            fold_window(&seed_value(None).expect("empty seed"), &ctxs).expect("fold to height 1");
        let (sapling, orchard, ironwood) = value_at_1.pool_treestates(&active(), height(1));
        let seed = Treestate {
            block_hash: BlockHash::from([1; 32]),
            height: height(1),
            time: 1_001,
            sapling,
            orchard,
            ironwood,
        };
        head.window_treestate(Some(&seed), active(), height(3))
            .await
            .expect("read succeeds");
        assert_eq!(frontiers.len(), 2, "heights 0 and 1 are the store's now");
    }

    /// Subtree completions are computed once per block and then served from
    /// the cache with the same answer.
    #[tokio::test]
    async fn subtree_roots_are_served_from_the_cache() {
        let frontiers = Arc::new(WindowFrontiers::default());
        let head = HeadSnapshot::new(Arc::new(funded_chain(3)), Arc::clone(&frontiers));
        let first = head
            .window_subtree_roots(None, ShieldedPool::Sapling, 0, None)
            .await
            .expect("read succeeds");
        assert_eq!(frontiers.len(), 3, "the whole window is folded");
        let again = head
            .window_subtree_roots(None, ShieldedPool::Sapling, 0, None)
            .await
            .expect("read succeeds");
        assert_eq!(first, again);
    }
}
