//! Tests over a mock validator.
//!
//! The mock is local to this crate rather than shared: ChainHead's driven port
//! is six questions, and a purpose-built fake that can be told to reorg, to
//! stall, or to move its tip mid-reconcile is worth more here than a general
//! harness.
//!
//! Two styles, chosen per test:
//!
//! - **Through the running service** — spawn it for real and observe the
//!   subscriber with [`wait_for`], as `zaino-mempool-rpc` does. This is what
//!   proves the writer task, its wake handling and its backoff actually work.
//! - **Stepped** — `spawn_without_writer` plus `advance_once`, for graph
//!   transitions where precise stepping beats polling. With no writer running
//!   the test is the only thing advancing the graph, so what it observes is
//!   exactly what it caused.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use zaino_chain_head::{
    AnchoredRelativeChainWork, ChainHeadBlockService as _, ChainHeadConfig,
    ChainHeadFreezeEvents as _, ChainHeadSnapshot as _,
};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    Block, BlockCommitments, BlockHash, BlockHeader, BlockRef, ChainMetadata, EquihashSolution,
    Height, MerkleRoot, TreeRoots,
};
use zaino_source::{
    FailureMode, FetchError, GetBlockByHashError, GetBlockError, GetChainTipError,
    GetChainTipsError, GetCommitmentTreeRootsError, OneShotGetBlock, OneShotGetBlockByHash,
    OneShotGetChainTip, OneShotGetChainTips, OneShotGetCommitmentTreeRoots, QueryError,
    SubscribeBlocks,
};

use crate::{service::ChainHeadService, snapshot::MapBackedSnapshot};

/// A valid nBits value: non-negative, non-zero, no overflow.
const VALID_BITS: u32 = 0x2007_ffff;

fn hash(id: u16) -> BlockHash {
    let mut bytes = [0; 32];
    bytes[..2].copy_from_slice(&id.to_le_bytes());
    BlockHash::from(bytes)
}

/// The id a test hash was built from, so a chain can be walked by id.
fn id_of(hash: &BlockHash) -> u16 {
    let bytes = <[u8; 32]>::from(*hash);
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("test height in range")
}

/// A block identified by a small integer, so test chains read as `1 -> 2 -> 3`.
fn block(h: u32, id: u16, parent: u16) -> Block {
    Block {
        header: BlockHeader {
            hash: hash(id),
            version: 4,
            prev_hash: hash(parent),
            height: height(h),
            time: 0,
            merkle_root: MerkleRoot::from([0; 32]),
            block_commitments: BlockCommitments::from([0; 32]),
            bits: VALID_BITS,
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: vec![],
        chain_metadata: ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        },
    }
}

#[derive(Default)]
struct MockState {
    /// Every block the validator knows, canonical or not.
    blocks: HashMap<BlockHash, Block>,
    /// The best chain, indexed by height.
    best_chain: Vec<BlockHash>,
    /// Fail this many more calls before answering normally.
    fail_calls: usize,
}

/// A validator whose chain the test controls.
#[derive(Clone)]
struct MockValidator {
    state: Arc<Mutex<MockState>>,
}

impl MockValidator {
    /// A best chain of `len` blocks, ids `0..len`, block `n` at height `n`.
    fn linear(len: u16) -> Self {
        let mut state = MockState::default();
        for id in 0..len {
            let h = u32::from(id);
            let parent = id.saturating_sub(1);
            state.blocks.insert(hash(id), block(h, id, parent));
            state.best_chain.push(hash(id));
        }
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state mutex poisoned")
    }

    /// Appends one block to the best chain.
    fn extend(&self, id: u16) {
        let mut state = self.lock();
        let h = u32::try_from(state.best_chain.len()).expect("test chain height in range");
        let parent = state.best_chain.last().map(id_of).unwrap_or(0);
        state.blocks.insert(hash(id), block(h, id, parent));
        state.best_chain.push(hash(id));
    }

    /// Replaces the best chain from `from_height` upwards with `ids`, leaving
    /// the displaced blocks known but no longer canonical.
    fn reorg(&self, from_height: u32, ids: &[u16]) {
        let mut state = self.lock();
        state.best_chain.truncate(from_height as usize);
        for (offset, &id) in ids.iter().enumerate() {
            let h = from_height + offset as u32;
            let parent = state.best_chain.last().map(id_of).unwrap_or(0);
            state.blocks.insert(hash(id), block(h, id, parent));
            state.best_chain.push(hash(id));
        }
    }

    fn tip(&self) -> (BlockHash, Height) {
        let state = self.lock();
        let index = state.best_chain.len() - 1;
        let h = u32::try_from(index).expect("test chain height in range");
        (state.best_chain[index], height(h))
    }
}

fn transport_failure<E: std::fmt::Debug + std::fmt::Display>() -> QueryError<E> {
    QueryError::Fetch(FetchError::new(FailureMode::Connection, "mock is down"))
}

impl OneShotGetChainTip for MockValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        {
            let mut state = self.lock();
            if state.fail_calls > 0 {
                state.fail_calls -= 1;
                return Err(transport_failure());
            }
        }
        Ok(self.tip())
    }
}

impl OneShotGetChainTips for MockValidator {
    async fn get_chain_tips(&self) -> Result<Vec<ChainTip>, QueryError<GetChainTipsError>> {
        let state = self.lock();
        let active_index = state.best_chain.len() - 1;
        let active_height = u32::try_from(active_index).expect("test chain height in range");
        let tips = vec![ChainTip {
            height: height(active_height),
            hash: state.best_chain[active_index],
            branch_len: 0,
            status: ChainTipStatus::Active,
        }];
        Ok(tips)
    }
}

impl OneShotGetBlock for MockValidator {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        let state = self.lock();
        state
            .best_chain
            .get(u32::from(height) as usize)
            .and_then(|hash| state.blocks.get(hash))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetBlockByHash for MockValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.lock()
            .blocks
            .get(&hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetCommitmentTreeRoots for MockValidator {
    async fn get_commitment_tree_roots(
        &self,
        _block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        Ok(TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        })
    }
}

impl SubscribeBlocks for MockValidator {}

/// A config that keeps the background task asleep, so tests step the runtime
/// themselves.
/// A config for stepped tests: the writer never runs, so the interval is set
/// long enough that nothing fires even if one is started by accident.
///
/// `ChainHeadConfig`'s fields are private, so it is built through its
/// constructor and adjusted through setters — which is also what a consumer
/// outside the crate must do.
fn test_config(max_depth: u32) -> ChainHeadConfig {
    let mut config = ChainHeadConfig::with_max_depth(nonzero_u32(max_depth));
    config.set_poll_interval_ms(nonzero_u64(3_600_000));
    config.set_initial_backoff_ms(nonzero_u64(1));
    config.set_max_backoff_ms(nonzero_u64(1));
    config.set_max_consecutive_failures(nonzero_u32(3));
    config
}

fn nonzero_u32(value: u32) -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(value).expect("test config values are not zero")
}

fn nonzero_u64(value: u64) -> std::num::NonZeroU64 {
    std::num::NonZeroU64::new(value).expect("test config values are not zero")
}

/// A config for tests that run the real writer task and poll for the result.
fn running_config(max_depth: u32) -> ChainHeadConfig {
    let mut config = test_config(max_depth);
    config.set_poll_interval_ms(nonzero_u64(2));
    config
}

/// An anchored chain head with no writer, for stepped tests.
async fn stepped(
    validator: &MockValidator,
    max_depth: u32,
) -> Arc<ChainHeadService<MockValidator>> {
    ChainHeadService::spawn_without_writer(
        Arc::new(validator.clone()),
        test_config(max_depth),
        CancellationToken::new(),
    )
    .await
    .expect("mock validator is reachable")
}

/// A chain head with its writer running, for behaviour tests.
async fn running(
    validator: &MockValidator,
    max_depth: u32,
) -> Arc<ChainHeadService<MockValidator>> {
    ChainHeadService::spawn(
        Arc::new(validator.clone()),
        running_config(max_depth),
        CancellationToken::new(),
    )
    .await
    .expect("mock validator is reachable")
}

/// Polls the subscriber until the predicate holds, as `zaino-mempool-rpc` does.
///
/// Bounded so a genuine failure surfaces as a panic rather than a hang.
async fn wait_for(
    service: &ChainHeadService<MockValidator>,
    what: &str,
    predicate: impl Fn(&MapBackedSnapshot) -> bool,
) -> Arc<MapBackedSnapshot> {
    let subscriber = service.subscriber();
    for _ in 0..1000 {
        let snapshot = subscriber.current();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("chain head never satisfied: {what}");
}

/// Advances the stepped service to the source's current tip.
async fn step_to_tip(service: &ChainHeadService<MockValidator>, validator: &MockValidator) {
    let _ = validator.tip();
    service.advance_once().await.expect("advance succeeds");
}

// ---------------------------------------------------------------- anchoring

/// Spawning anchors at `tip - depth` and nothing more: the writer extends from
/// there one block at a time, as the non-finalised state always did.
#[tokio::test]
async fn spawn_anchors_at_the_window_floor() {
    let validator = MockValidator::linear(50);

    let service = stepped(&validator, 10).await;
    let snapshot = service.subscriber().current();

    assert_eq!(snapshot.best_tip().height, height(39));
    assert_eq!(snapshot.retained_block_count(), 1);
}

/// A chain shorter than the depth anchors at genesis.
#[tokio::test]
async fn a_short_chain_anchors_at_genesis() {
    let validator = MockValidator::linear(5);

    let service = stepped(&validator, 100).await;

    assert_eq!(service.subscriber().current().best_tip().height, height(0));
}

/// The anchor is the floor's parent: the block work is counted from, which the
/// graph never retains.
///
/// This is what makes rebasing to absolute chainwork a plain addition —
/// `absolute(B) = chainwork(anchor) + work(B)` — with no subtraction and no
/// double-count of the floor's own work.
#[tokio::test]
async fn the_work_anchor_is_the_parent_of_the_window_floor() {
    let validator = MockValidator::linear(50);

    let service = stepped(&validator, 10).await;
    let snapshot = service.subscriber().current();

    assert_eq!(snapshot.best_tip().height, height(39), "the floor");
    assert_eq!(
        snapshot.work_anchor(),
        Some(BlockRef {
            hash: hash(38),
            height: height(38),
        }),
    );
    assert!(
        snapshot.block_by_hash(&hash(38)).is_none(),
        "the anchor is below the window, not the lowest block in it",
    );
}

/// The floor's work is exactly its own block work, because the anchor below it
/// contributes none.
#[tokio::test]
async fn the_floor_holds_exactly_its_own_block_work() {
    let validator = MockValidator::linear(50);

    let service = stepped(&validator, 10).await;
    let snapshot = service.subscriber().current();
    let floor = snapshot
        .best_block_by_height(height(39))
        .expect("the floor is retained");

    assert_eq!(
        floor.work,
        AnchoredRelativeChainWork::ZERO
            .checked_add(zaino_primitives::types::ChainWork::from_u128(
                zaino_consensus::work_from_bits(VALID_BITS).expect("valid nBits"),
            ))
            .expect("one block cannot overflow"),
    );
}

/// A chain shorter than the depth floors at genesis, which has no parent — so
/// there is no anchor and the work is already absolute.
#[tokio::test]
async fn a_genesis_floor_has_no_work_anchor() {
    let validator = MockValidator::linear(5);

    let service = stepped(&validator, 100).await;

    assert_eq!(service.subscriber().current().work_anchor(), None);
}

/// Retention prunes the floor block itself, and the anchor outlives it.
///
/// This is why the anchor is recorded rather than derived from the graph. Every
/// surviving block's work still counts from the same place after the floor is
/// gone, and the lowest *retained* block is no substitute — its work is an
/// accumulation from the anchor, not its own block work, so rebasing against it
/// would count the blocks between them twice.
#[tokio::test]
async fn the_work_anchor_survives_the_floor_being_pruned() {
    let validator = MockValidator::linear(50);
    let service = stepped(&validator, 10).await;
    step_to_tip(&service, &validator).await;

    // Far enough past the floor that trimming — which stops `max_depth` plus
    // the retention margin below the tip — reaches above it. In two passes, so
    // the held tip never falls below the floor the next tick computes: that
    // would re-anchor the graph, and a re-anchored graph has a new anchor
    // rather than an outlived one, which is the opposite of what is under test.
    for id in 50..60 {
        validator.extend(id);
    }
    step_to_tip(&service, &validator).await;
    for id in 60..70 {
        validator.extend(id);
    }
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert!(
        snapshot.best_block_by_height(height(39)).is_none(),
        "the floor block must actually have been pruned for this to test anything",
    );
    assert_eq!(
        snapshot.work_anchor(),
        Some(BlockRef {
            hash: hash(38),
            height: height(38),
        }),
        "the anchor is the same block it always was",
    );
}

/// Re-anchoring moves the anchor, because the new graph accumulates from a new
/// floor.
///
/// The companion to
/// [`the_work_anchor_survives_the_floor_being_pruned`]: the anchor outlives
/// pruning but not a rebuild, so a consumer caching the anchor's absolute
/// chainwork must key that cache to the snapshot rather than hold it for the
/// life of the chain head.
#[tokio::test]
async fn a_re_anchor_moves_the_work_anchor() {
    let validator = MockValidator::linear(50);
    let service = stepped(&validator, 10).await;
    step_to_tip(&service, &validator).await;
    let before = service.subscriber().current().work_anchor();

    // One jump larger than the window, so the held tip falls below the floor
    // the next tick computes and the graph is rebuilt rather than extended.
    for id in 50..75 {
        validator.extend(id);
    }
    step_to_tip(&service, &validator).await;

    let after = service.subscriber().current().work_anchor();
    assert_ne!(
        before, after,
        "a rebuilt graph accumulates from a new floor"
    );
    assert_eq!(
        after,
        Some(BlockRef {
            hash: hash(63),
            height: height(63),
        }),
        "the parent of the new floor at `tip - max_depth`",
    );
}

/// A validator unreachable at startup fails construction rather than producing
/// a chain head with nothing in it.
#[tokio::test]
async fn spawn_fails_when_the_validator_never_answers() {
    let validator = MockValidator::linear(5);
    validator.lock().fail_calls = usize::MAX;

    let error = ChainHeadService::spawn(
        Arc::new(validator),
        test_config(100),
        CancellationToken::new(),
    )
    .await
    .expect_err("unreachable validator must fail construction");

    assert!(matches!(
        error,
        crate::ChainHeadInitError::SourceUnavailable { .. }
    ));
}

/// A briefly-unreachable validator is retried rather than treated as fatal.
#[tokio::test]
async fn spawn_retries_a_transient_failure() {
    let validator = MockValidator::linear(5);
    validator.lock().fail_calls = 2;

    let service = stepped(&validator, 100).await;

    assert_eq!(service.subscriber().current().best_tip().height, height(0));
}

// ------------------------------------------------------- graph transitions

#[tokio::test]
async fn advancing_extends_to_the_chain_tip() {
    let validator = MockValidator::linear(10);
    let service = stepped(&validator, 100).await;

    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(9));
    assert_eq!(snapshot.best_tip().hash, hash(9));
    assert_eq!(snapshot.retained_block_count(), 10);
}

/// Work accumulates from the anchor, so a later block always outweighs an
/// earlier one. Ordering is the only property chain selection relies on.
#[tokio::test]
async fn work_accumulates_along_the_chain() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    let first = snapshot.best_block_by_height(height(0)).expect("anchor");
    let last = snapshot.best_block_by_height(height(4)).expect("tip");
    assert!(last.work > first.work);
}

/// A reorg to a longer chain. The displaced block stays retained — it is a
/// competing block now — but is no longer canonical at its height.
///
/// This is the only way the chain head learns of a competing branch: it lived
/// through the reorg that created one.
#[tokio::test]
async fn a_higher_reorg_retains_the_displaced_branch() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;

    validator.reorg(4, &[40, 41]);
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(5));
    assert_eq!(snapshot.best_tip().hash, hash(41));
    assert_eq!(
        snapshot.best_block_by_height(height(4)).map(|b| b.hash()),
        Some(hash(40))
    );
    let displaced = snapshot
        .block_by_hash(&hash(4))
        .expect("the displaced block is retained");
    assert!(!snapshot.is_on_best_chain(displaced.reference));
}

/// A branch swap at the same height. The extension loop cannot see this — it
/// finds no higher block — so `check_for_nonhigher_reorgs` is what catches it.
#[tokio::test]
async fn a_same_height_reorg_is_caught_without_a_higher_block() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;

    validator.reorg(4, &[40]);
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(4));
    assert_eq!(snapshot.best_tip().hash, hash(40));
}

/// Growth past the window drops the oldest blocks, so retention stays bounded
/// rather than accumulating one block per new block.
#[tokio::test]
async fn the_window_stays_bounded_as_the_chain_grows() {
    let validator = MockValidator::linear(40);
    let service = stepped(&validator, 5).await;
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(39));
    // Trimming keeps a margin below the configured depth so it never cuts
    // inside the reorg-possible range, so the window is bounded but not
    // exactly `depth` blocks.
    assert!(
        snapshot.retained_block_count() <= 17,
        "window grew to {} blocks",
        snapshot.retained_block_count(),
    );
}

// ----------------------------------------------------------- publication

/// A subscriber held across a publish must see the new snapshot.
///
/// Taking a fresh subscriber after each step cannot distinguish a handle that
/// reads the published cell from one that captured a snapshot when it was made.
/// Every real consumer holds one for its lifetime.
#[tokio::test]
async fn a_held_subscriber_observes_new_snapshots() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;

    let subscriber = service.subscriber();
    assert_eq!(subscriber.current().best_tip().height, height(0));

    step_to_tip(&service, &validator).await;

    assert_eq!(
        subscriber.current().best_tip().height,
        height(4),
        "a subscriber created before the advance must observe its result",
    );
}

/// The epoch identifies chain state, so it advances on a tip change and stays
/// put when nothing moved.
#[tokio::test]
async fn the_epoch_advances_only_when_the_tip_changes() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;
    let first = service.subscriber().epoch();

    step_to_tip(&service, &validator).await;
    assert_eq!(service.subscriber().epoch(), first);

    validator.extend(5);
    step_to_tip(&service, &validator).await;

    let second = service.subscriber().epoch();
    assert_eq!(second.generation, first.generation + 1);
    assert_ne!(second.best_tip, first.best_tip);
}

/// The generation keeps advancing across a re-anchor.
///
/// A re-anchored graph is built from a single block and starts at generation
/// zero, so a rule that inherited from the graph it replaces would let a
/// generation repeat — and a consumer holding the earlier epoch would be told
/// its stale view was current.
#[tokio::test]
async fn the_generation_advances_across_a_re_anchor() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 5).await;
    step_to_tip(&service, &validator).await;
    let before = service.subscriber().epoch();

    // Far enough past the window that the writer re-anchors rather than walking
    // the gap one block at a time. The next tick anchors at `tip - max_depth`,
    // so the held tip has to fall below that for the re-anchor branch to be
    // taken at all — asserted, so this cannot quietly become a test of the
    // ordinary extension path.
    validator.extend(100);
    assert!(
        u32::from(before.best_tip.height) < 105 - 5,
        "the held tip must be below the anchor the next tick computes",
    );
    step_to_tip(&service, &validator).await;

    let after = service.subscriber().epoch();
    assert!(
        after.generation > before.generation,
        "a re-anchor must not reset or repeat the generation: {} -> {}",
        before.generation,
        after.generation,
    );
    assert_eq!(
        service.subscriber().current().epoch(),
        after,
        "the re-anchored snapshot carries the epoch that was published",
    );
}

/// A captured snapshot reports the epoch it was published under, not whatever
/// the chain head has moved on to since.
///
/// This is the property the mempool's coherence layer rests on: it compares a
/// caller's snapshot against the transaction set's epoch, so a snapshot that
/// reported the *handle's* current epoch would claim coherence with a tip the
/// caller never saw.
#[tokio::test]
async fn a_snapshot_keeps_the_epoch_it_was_published_under() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;

    let captured = service.subscriber().current();
    let captured_epoch = captured.epoch();
    assert_eq!(captured_epoch, service.subscriber().epoch());

    validator.extend(5);
    step_to_tip(&service, &validator).await;

    assert_eq!(
        captured.epoch(),
        captured_epoch,
        "the captured view's epoch must not follow the chain head forward",
    );
    assert_ne!(service.subscriber().epoch(), captured_epoch);
    assert_eq!(
        service.subscriber().current().epoch(),
        service.subscriber().epoch(),
        "a freshly captured view agrees with the handle",
    );
}

/// A failed advance leaves the last published snapshot in place: stale data
/// with a status saying so beats no data.
#[tokio::test]
async fn a_failed_advance_leaves_the_snapshot_intact() {
    let validator = MockValidator::linear(5);
    let service = stepped(&validator, 100).await;
    step_to_tip(&service, &validator).await;
    let before = service.subscriber().current().best_tip();

    validator.lock().fail_calls = usize::MAX;
    let _ = service.advance_once().await;

    assert_eq!(service.subscriber().current().best_tip(), before);
}

// ------------------------------------------------------------ freeze handoff

/// Blocks are handed off once they pass below the consensus seam, ascending and
/// contiguous.
#[tokio::test]
async fn frozen_blocks_are_emitted_in_order_below_the_seam() {
    let validator = MockValidator::linear(20);
    let service = stepped(&validator, 5).await;
    let mut frozen = service.subscriber().subscribe_frozen();

    step_to_tip(&service, &validator).await;

    // Tip 19, depth 5, so everything at or below 14 is frozen. The anchor sits
    // at 14, and the graph only holds 14 upwards, so 14 is where it starts.
    let mut heights = Vec::new();
    while let Ok(block) = frozen.try_recv() {
        heights.push(u32::from(block.height()));
    }
    assert!(!heights.is_empty(), "nothing was handed off");
    assert!(
        heights.windows(2).all(|w| w[1] == w[0] + 1),
        "handoff was not contiguous and ascending: {heights:?}",
    );
    assert!(
        heights.iter().all(|h| *h <= 14),
        "a block still inside the reorg-possible range was handed off: {heights:?}",
    );
}

/// A block is handed off once, not on every publish that follows.
#[tokio::test]
async fn a_frozen_block_is_emitted_only_once() {
    let validator = MockValidator::linear(20);
    let service = stepped(&validator, 5).await;
    let mut frozen = service.subscriber().subscribe_frozen();

    step_to_tip(&service, &validator).await;
    let mut seen = Vec::new();
    while let Ok(block) = frozen.try_recv() {
        seen.push(u32::from(block.height()));
    }

    // An advance that moves nothing must hand off nothing.
    service.advance_once().await.expect("advance succeeds");
    assert!(
        frozen.try_recv().is_err(),
        "an advance with no tip change handed off a block again",
    );

    validator.extend(20);
    service.advance_once().await.expect("advance succeeds");
    let next = frozen.try_recv().expect("one more block crossed the seam");
    assert_eq!(
        u32::from(next.height()),
        seen.last().expect("some were handed off") + 1,
    );
}

// ----------------------------------------------------------- the writer task

/// The writer task reaches the tip on its own, without anything stepping it.
///
/// The stepped tests above never exercise `run`, its wake handling or its
/// backoff; this is what proves the runtime works when nothing is driving it.
#[tokio::test]
async fn the_writer_task_reaches_the_tip_unaided() {
    let validator = MockValidator::linear(10);
    let service = running(&validator, 100).await;

    wait_for(&service, "the chain tip", |snapshot| {
        snapshot.best_tip().height == height(9)
    })
    .await;
}

/// The writer follows the chain as it grows.
#[tokio::test]
async fn the_writer_task_follows_new_blocks() {
    let validator = MockValidator::linear(5);
    let service = running(&validator, 100).await;
    wait_for(&service, "the initial tip", |s| {
        s.best_tip().height == height(4)
    })
    .await;

    validator.extend(5);
    validator.extend(6);

    wait_for(&service, "the extended tip", |s| {
        s.best_tip().height == height(6)
    })
    .await;
}

#[tokio::test]
async fn shutdown_reports_closing() {
    let validator = MockValidator::linear(5);
    let service = running(&validator, 100).await;
    wait_for(&service, "readiness", |s| s.best_tip().height == height(4)).await;

    service.shutdown();

    assert_eq!(
        service.status().lifecycle,
        zaino_component::Lifecycle::Closing
    );
}

/// The subscriber reads the runtime's status, not a copy taken when it was made.
///
/// A snapshot looks the same whether the writer is keeping up or has given up,
/// so a consumer holding only the read handle needs this to know whether the
/// tip it is being served is fresh. Asserting across a transition is what
/// proves the two handles share one cell rather than merely agreeing once.
#[tokio::test]
async fn the_subscriber_observes_status_transitions() {
    use zaino_component::{Lifecycle, StatusSource as _};

    let validator = MockValidator::linear(5);
    let service = running(&validator, 100).await;
    let subscriber = service.subscriber();
    wait_for(&service, "readiness", |s| s.best_tip().height == height(4)).await;

    assert_eq!(subscriber.status().lifecycle, Lifecycle::Ready);

    service.shutdown();

    assert_eq!(subscriber.status().lifecycle, Lifecycle::Closing);
    assert_eq!(subscriber.status(), service.status());
}
