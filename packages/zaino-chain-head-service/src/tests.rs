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

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadConfig, ChainHeadSnapshot as _};
use zaino_component::{RunLoop as _, RunReporter};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, EquihashSolution, Height,
    MerkleRoot, TreeRoots,
};
use zaino_source::{
    FailureMode, GetBlockByHashError, GetBlockError, GetChainTipError, GetChainTipsError,
    GetCommitmentTreeRootsError, NonDomainError, OneShotGetBlock, OneShotGetBlockByHash,
    OneShotGetChainTip, OneShotGetChainTips, OneShotGetCommitmentTreeRoots, QueryError,
    SubscribeBlocks,
};

use crate::{service::ChainHeadService, snapshot::MapBackedSnapshot};

/// A valid nBits value: non-negative, non-zero, no overflow.
fn valid_bits() -> zaino_primitives::types::CompactDifficulty {
    zaino_primitives::types::CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits")
}

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
            bits: valid_bits(),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: vec![],
        chain_metadata: ChainMetadata::ZERO,
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
    QueryError::NonDomain(NonDomainError::new(FailureMode::Connection, "mock is down"))
}

impl zaino_source::ValidatorSource for MockValidator {
    type NonDomain = zaino_source::NonDomainError;
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

/// A confirmed-watermark receiver fixed at `value` for its whole life.
///
/// The sender is dropped, so `borrow()` keeps returning `value`: a test that
/// does not exercise the confirm-before-trim handshake supplies a fixed floor
/// and reads the graph transitions alone. `None` — the finalised store holds
/// nothing — is the value that keeps trimming down to genesis, which is what
/// most graph tests want when their depth already saturates the floor.
fn fixed_watermark(value: Option<Height>) -> watch::Receiver<Option<Height>> {
    watch::channel(value).1
}

/// An anchored chain head with no writer, for stepped tests.
async fn stepped(
    validator: &MockValidator,
    max_depth: u32,
) -> Arc<ChainHeadService<MockValidator>> {
    stepped_with_watermark(validator, max_depth, fixed_watermark(None)).await
}

/// A stepped chain head whose confirmed watermark the test controls.
async fn stepped_with_watermark(
    validator: &MockValidator,
    max_depth: u32,
    confirmed_watermark: watch::Receiver<Option<Height>>,
) -> Arc<ChainHeadService<MockValidator>> {
    ChainHeadService::spawn_without_writer(
        Arc::new(validator.clone()),
        test_config(max_depth),
        confirmed_watermark,
        CancellationToken::new(),
    )
    .await
    .expect("mock validator is reachable")
}

/// A chain head with its writer loop driven, for behaviour tests.
///
/// Mirrors how the runtime boots it — anchor, then drive the [`RunLoop`] — but
/// spawns the loop directly rather than through a `RunComponent`, so the crate's
/// tests stay free of the runtime. The loop's own cancel token is never
/// cancelled; the task is torn down with the test's runtime, and
/// [`ChainHeadService::shutdown`] is what the shutdown tests exercise.
async fn running(
    validator: &MockValidator,
    max_depth: u32,
) -> Arc<ChainHeadService<MockValidator>> {
    let (_subscriber, writer) = ChainHeadService::anchor(
        Arc::new(validator.clone()),
        running_config(max_depth),
        fixed_watermark(None),
        CancellationToken::new(),
    )
    .await
    .expect("mock validator is reachable");
    let service = Arc::new(writer);
    let runner = Arc::clone(&service);
    tokio::spawn(async move {
        let _ = runner
            .run(CancellationToken::new(), RunReporter::new(|_| {}))
            .await;
    });
    service
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

/// A validator unreachable at startup fails construction rather than producing
/// a chain head with nothing in it.
#[tokio::test]
async fn spawn_fails_when_the_validator_never_answers() {
    let validator = MockValidator::linear(5);
    validator.lock().fail_calls = usize::MAX;

    let error = ChainHeadService::anchor(
        Arc::new(validator),
        test_config(100),
        fixed_watermark(None),
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

/// With a finalised store keeping up — its confirmed watermark tracking
/// `tip - max_depth` — retention stays bounded rather than accumulating one
/// block per new block.
///
/// The watermark is what admits trimming here: without it the non-finalised
/// head must retain everything the finalised side has not confirmed. A store
/// that stays a window behind the tip lets the floor rise to
/// `watermark - RETENTION_MARGIN`, reproducing the old tip-relative bound.
#[tokio::test]
async fn the_window_stays_bounded_when_the_store_keeps_up() {
    let validator = MockValidator::linear(40);
    // A keeping-up finalised store confirms up to `tip - max_depth` (39 - 5).
    let watermark = fixed_watermark(Some(height(34)));
    let service = stepped_with_watermark(&validator, 5, watermark).await;
    step_to_tip(&service, &validator).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(39));
    // Floor is `min(tip - max_depth, watermark - margin)` = min(34, 24) = 24,
    // so heights 24..=39 are retained: 16 blocks, bounded but not exactly
    // `depth` because the confirmation overlap keeps a few more.
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

// ------------------------------------------------ confirm-before-trim floor

/// Grows the source one block at a time, stepping the head after each, so the
/// graph tracks the tip without ever re-anchoring — the path where the trim
/// floor, not the anchor floor, decides what is retained.
async fn grow_stepping(
    service: &ChainHeadService<MockValidator>,
    validator: &MockValidator,
    next_ids: impl IntoIterator<Item = u16>,
) {
    for id in next_ids {
        validator.extend(id);
        service.advance_once().await.expect("advance succeeds");
    }
}

/// A finalised store lagging behind the reorg window holds the floor down: the
/// non-finalised head must not trim a height the store has not yet confirmed,
/// even one already outside the tip-relative reorg window.
///
/// The graph anchors at genesis and grows to tip 20 with depth 5, so the
/// reorg-safety floor alone would trim everything below 15. The store has
/// confirmed only height 5, so the confirmation floor saturates to genesis and
/// wins: heights below the tip-relative window — 10, say — stay retained
/// because the store cannot yet serve them.
#[tokio::test]
async fn a_lagging_watermark_retains_below_the_tip_relative_window() {
    let validator = MockValidator::linear(6);
    let watermark = fixed_watermark(Some(height(5)));
    let service = stepped_with_watermark(&validator, 5, watermark).await;
    step_to_tip(&service, &validator).await;

    grow_stepping(&service, &validator, 6..=20).await;

    let snapshot = service.subscriber().current();
    assert_eq!(snapshot.best_tip().height, height(20));
    // Height 10 is below the tip-relative window floor (20 - 5 = 15) yet still
    // retained: the lagging watermark forbids trimming what the store has not
    // confirmed.
    assert!(
        snapshot.best_block_by_height(height(10)).is_some(),
        "a height below the tip-relative window was trimmed before the store \
         confirmed it",
    );
}

/// An advancing watermark lets the floor track it: as the finalised store
/// confirms more, the non-finalised head trims up to `watermark - margin`, and
/// no further.
#[tokio::test]
async fn an_advancing_watermark_moves_the_trim_floor_to_the_overlap() {
    let validator = MockValidator::linear(6);
    let (sender, receiver) = watch::channel(Some(height(0)));
    let service = stepped_with_watermark(&validator, 5, receiver).await;
    step_to_tip(&service, &validator).await;
    grow_stepping(&service, &validator, 6..=30).await;

    // Watermark 0: floor is `min(25, 0 - margin)` = genesis, so a height far
    // below the tip-relative window is still retained.
    assert!(
        service
            .subscriber()
            .current()
            .best_block_by_height(height(10))
            .is_some(),
        "the head trimmed below the confirmed watermark",
    );

    // The store confirms up to 25. The floor becomes `min(31 - 5, 25 - 10)` =
    // 15, so the next tip-changing tick drops everything below 15 and keeps the
    // overlap at and above it.
    sender.send_replace(Some(height(25)));
    grow_stepping(&service, &validator, [31]).await;

    let snapshot = service.subscriber().current();
    assert!(
        snapshot.best_block_by_height(height(14)).is_none(),
        "a height below `watermark - margin` survived after the store confirmed \
         past it",
    );
    assert!(
        snapshot.best_block_by_height(height(15)).is_some(),
        "the confirmation overlap was not kept: height 15 should survive",
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

    assert_eq!(service.status(), zaino_status::StatusType::Closing);
}

/// The subscriber reads the runtime's status, not a copy taken when it was made.
///
/// A snapshot looks the same whether the writer is keeping up or has given up,
/// so a consumer holding only the read handle needs this to know whether the
/// tip it is being served is fresh. Asserting across a transition is what
/// proves the two handles share one cell rather than merely agreeing once.
#[tokio::test]
async fn the_subscriber_observes_status_transitions() {
    use zaino_status::{Status as _, StatusType};

    let validator = MockValidator::linear(5);
    let service = running(&validator, 100).await;
    let subscriber = service.subscriber();
    wait_for(&service, "readiness", |s| s.best_tip().height == height(4)).await;

    assert_eq!(subscriber.status(), StatusType::Ready);

    service.shutdown();

    assert_eq!(subscriber.status(), StatusType::Closing);
    assert_eq!(subscriber.status(), service.status());
}
