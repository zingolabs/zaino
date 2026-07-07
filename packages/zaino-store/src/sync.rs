//! Sync loop — keeps a [`ChainState`] in sync with a remote source.
//!
//! The sync loop is generic over any [`BlockFetcher`] implementation.
//! It handles fork detection, reorg (via `add_fragment`), batch ingestion,
//! and freezing — but never touches network protocols or block serialisation
//! formats.

use std::sync::Arc;
use std::time::{Duration, Instant};

use im::Vector;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::SyncError;
use crate::fetcher::BlockFetcher;
use crate::state::ChainState;
use crate::types::{Block, Height, MAX_REORG_DEPTH};

/// Maximum blocks to fetch in a single sync_step iteration. Prevents the
/// initial sync from fetching millions of blocks in one blocking call.
const MAX_BATCH_SIZE: u32 = 1000;

// =========================================================================
// Configuration
// =========================================================================

/// Parameters for the block store sync loop.
#[derive(Clone, Copy, Debug)]
pub struct SyncTimings {
    /// Sleep duration between iterations when already synced.
    pub interval: Duration,
    /// Initial backoff on failure.
    pub initial_backoff: Duration,
    /// Maximum backoff on repeated failure.
    pub max_backoff: Duration,
    /// Consecutive failures before giving up.
    pub max_consecutive_failures: u32,
}

impl Default for SyncTimings {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
            max_consecutive_failures: 10,
        }
    }
}

// =========================================================================
// Sync loop runner
// =========================================================================

/// The block store sync loop. Bridges a [`BlockFetcher`] (data source) and
/// a [`ChainState`] (block store), keeping the store in sync with the
/// source's best chain.
pub struct BlockStoreSync<F: BlockFetcher> {
    state: Arc<ChainState>,
    fetcher: F,
    timings: SyncTimings,
    cancel_token: CancellationToken,
}

impl<F: BlockFetcher + Clone + Send + Sync + 'static> BlockStoreSync<F> {
    /// Create a new sync loop wrapper.
    pub fn new(state: Arc<ChainState>, fetcher: F, timings: SyncTimings) -> Self {
        Self {
            state,
            fetcher,
            timings,
            cancel_token: CancellationToken::new(),
        }
    }

    /// Start the background sync loop. Returns a [`tokio::task::JoinHandle`]
    /// that can be awaited or cancelled via [`Self::shutdown`].
    pub fn start_sync_loop(&self) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let mut fetcher = self.fetcher.clone();
        let timings = self.timings;
        let cancel = self.cancel_token.clone();
        tokio::task::spawn(async move {
            let sync_start = Instant::now();
            let start_height = state.tip_height();
            let mut initial_sync_logged = false;
            let mut consecutive_failures: u32 = 0;
            let mut current_backoff = timings.initial_backoff;

            loop {
                if cancel.is_cancelled() {
                    return;
                }

                match sync_step(&state, &mut fetcher).await {
                    Ok(()) => {
                        if !initial_sync_logged {
                            initial_sync_logged = true;
                            let elapsed = sync_start.elapsed();
                            let end_height = state.tip_height();
                            info!(
                                elapsed = format_duration(elapsed),
                                elapsed_secs = elapsed.as_secs_f64(),
                                start_height,
                                end_height,
                                "BlockStore initial sync complete"
                            );
                        }
                        consecutive_failures = 0;
                        current_backoff = timings.initial_backoff;
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(timings.interval) => {}
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= timings.max_consecutive_failures {
                            warn!(
                                "BlockStore sync loop failed {consecutive_failures} consecutive times, giving up: {e}"
                            );
                            return;
                        }
                        warn!(
                            "BlockStore sync step failed ({consecutive_failures}/{}): {e}",
                            timings.max_consecutive_failures
                        );
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(current_backoff) => {}
                        }
                        current_backoff = (current_backoff * 2).min(timings.max_backoff);
                    }
                }
            }
        })
    }

    /// Signal the sync loop to shut down.
    pub fn shutdown(&self) {
        self.cancel_token.cancel();
    }
}

// =========================================================================
// Helpers
// =========================================================================

/// Format a [`Duration`] as a human-readable string: `3h24m15s`, `45m2s`, `7s`.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{}s", secs);
    }
    let mins = secs / 60;
    let remaining_secs = secs % 60;
    if mins < 60 {
        return format!("{}m{}s", mins, remaining_secs);
    }
    let hours = mins / 60;
    let mins = mins % 60;
    format!("{}h{}m{}s", hours, mins, remaining_secs)
}

// =========================================================================
// Sync algorithm
// =========================================================================

/// One iteration of the sync algorithm.
///
/// Returns `Ok(())` if synced or progress was made, `Err(...)` on failure.
pub async fn sync_step<F: BlockFetcher + ?Sized>(
    state: &ChainState,
    fetcher: &mut F,
) -> Result<(), SyncError> {
    let iter_start = Instant::now();

    // 1. Fetch remote tip.
    let t0 = Instant::now();
    let (remote_hash, remote_height) = fetcher
        .fetch_tip()
        .await
        .map_err(|e| SyncError::Fetch(e.to_string()))?;
    let tip_rpc_ms = t0.elapsed().as_millis();

    // 2. Early exit: already synced.  Remote tip hash matches ours — nothing
    //    to do but trim.  Skips the expensive find_trim_index slow-sync walk
    //    (one RPC call per iteration) and the misleading "applied fragment" log.
    if remote_hash == state.tip() {
        state.trim_chain()?;
        return Ok(());
    }

    // 3. Compute gap: how far ahead (or behind, if negative) the remote is
    //    relative to our next-expected block height.
    let ct = state.ct();
    let gap = remote_height as i64 - ct as i64;

    // 4. Flush + forward fill when the remote is far ahead.
    if gap >= MAX_REORG_DEPTH as i64 {
        // Persist any in-memory chain blocks so we can write directly to
        // the freezer during forward fill.
        state.flush_chain_to_lmdb()?;

        // Bulk-fetch blocks from ct to remote_tip - D and write them
        // straight to LMDB — they never enter the in-memory chain.
        let to = remote_height.saturating_sub(MAX_REORG_DEPTH);
        loop {
            let cursor = state.ct();
            if cursor > to {
                break;
            }
            let forward_to = u32::min(to, cursor + MAX_BATCH_SIZE - 1);

            let batch_start_t = Instant::now();
            let blocks = fetcher
                .fetch_batch(cursor, forward_to)
                .await
                .map_err(|e| SyncError::Fetch(e.to_string()))?;
            let fetch_ms = batch_start_t.elapsed().as_millis();

            let batch_len = blocks.len();
            if batch_len == 0 {
                break;
            }

            let batch_start = cursor;
            state.append_to_freezer(&blocks)?;

            let batch_secs = batch_start_t.elapsed().as_secs_f64();
            let total_ms = Instant::now().duration_since(iter_start).as_millis();
            info!(
                from = batch_start,
                to = forward_to,
                count = batch_len,
                new_cs = state.chain_start(),
                tip_rpc_ms,
                fetch_ms,
                total_ms,
                blocks_per_s = (batch_len as f64 / batch_secs) as u32,
                "BlockStore forward fill wrote to LMDB"
            );
        }
    }

    // 5. Slow sync — walk backward from the remote tip to find the fork
    //    point and get any new blocks.  The early-exit above catches the
    //    already-synced case, so this path only runs when something changed.
    let ss_start = Instant::now();
    let (trim_from, fragment) =
        find_trim_index(state, fetcher, remote_height, MAX_REORG_DEPTH).await?;
    let ss_fetch_ms = ss_start.elapsed().as_millis();

    let fragment_len = fragment.len();
    state.append_to_chain(trim_from, fragment)?;

    let total_ms = Instant::now().duration_since(iter_start).as_millis();
    if fragment_len > 0 {
        info!(
            trim_from,
            fragment_len,
            new_tip = state.tip_height(),
            tip_rpc_ms,
            fetch_ms = ss_fetch_ms,
            total_ms,
            "BlockStore slow sync applied fragment"
        );
    }

    // 6. Trim: if the chain grew past MAX_REORG_DEPTH, freeze the excess
    //    from the head to LMDB.
    state.trim_chain()?;

    // Invariant: ct - cs == chain.len()
    debug_assert_eq!(state.ct() - state.chain_start(), {
        let chain = state.chain.read().unwrap();
        chain.len() as u32
    });
    // trim_chain enforces ct - cs <= D.
    debug_assert!(state.ct() - state.chain_start() <= MAX_REORG_DEPTH);

    Ok(())
}

/// Walk backward from `remote_tip_height`, fetching one block at a time,
/// accumulating a fragment and checking for the fork point.
///
/// Returns `(trim_from, fragment)` when the fork is found.  `trim_from` is
/// the first height to replace (inclusive): the caller keeps the local chain
/// up to `trim_from - 1` and appends `fragment`.  `trim_from = 0` means no
/// common ancestor — discard the entire local chain and start from genesis.
/// `trim_from = cs` means the fork is at the freezer boundary (the block at
/// `cs - 1` in LMDB is the common ancestor).
///
/// This mirrors the Lean `findTrimIndex` entry point: it initialises the
/// accumulator to empty and fuel to `MAX_REORG_DEPTH`, then delegates to
/// [`find_trim_index_int`] (the unrolled-loop translation of the
/// tail-recursive `findTrimIndexInt` in `docs/lean/Proof.lean`).
///
/// Errors with [`SyncError::ReorgTooDeep`] if fuel is exhausted without
/// finding a common ancestor, or [`SyncError::ChainIncoherent`] if the
/// boundary check fails (missing LMDB block, or bad `prev_hash` at
/// genesis) or if the fetched fragment is internally incoherent
/// (`fragment[i].hash ≠ fragment[i+1].prev_hash`).
async fn find_trim_index<F: BlockFetcher + ?Sized>(
    state: &ChainState,
    fetcher: &mut F,
    remote_tip_height: Height,
    fuel: u32,
) -> Result<(Height, Vector<Block>), SyncError> {
    // Lean: findTrimIndex cs rtip freezer chain fetchRemote [] D genesisHash
    //   → findTrimIndexInt rtip cs freezer chain fetchRemote [] D genesisHash
    let (trim_from, fragment) =
        find_trim_index_int(state, fetcher, remote_tip_height, Vector::new(), fuel, fuel).await?;

    // Validate internal contiguity of the fragment (Lean: List.IsChain).
    // The boundary link — fragment[0].prev_hash against the local block at
    // trim_from - 1 — is already verified inside find_trim_index_int (it is
    // the fork-detection condition that determined trim_from).  Only the
    // fragment-internal links remain: fragment[i].hash == fragment[i+1].prev_hash.
    for i in 0..fragment.len().saturating_sub(1) {
        let cur = &fragment[i];
        let next = &fragment[i + 1];
        if cur.hash != next.prev_hash {
            return Err(SyncError::ChainIncoherent {
                height: next.height,
                expected: next.prev_hash,
                got: cur.hash,
            });
        }
    }

    Ok((trim_from, fragment))
}

/// Unrolled-loop translation of the tail-recursive Lean `findTrimIndexInt`.
///
/// Walks backward from `h`, accumulates a fragment in `fragment`, and stops
/// at the first height where the remote block builds on top of our local data
/// (chain[h-1], or freezer[cs-1], or genesis).  It does NOT validate internal
/// contiguity of the accumulated fragment — that is left to the caller
/// (matching the Lean model, where `findTrimIndexInt` omits `List.IsChain`
/// to simplify the proofs; the caller is expected to verify and discard on
/// failure without mutating state).
///
/// `depth` is the original fuel value, carried for the [`SyncError::ReorgTooDeep`]
/// diagnostic.
async fn find_trim_index_int<F: BlockFetcher + ?Sized>(
    state: &ChainState,
    fetcher: &mut F,
    mut h: Height,
    mut fragment: Vector<Block>,
    mut fuel: u32,
    depth: u32,
) -> Result<(Height, Vector<Block>), SyncError> {
    loop {
        // Lean: match fuel with 0 => .error .fuelExhausted
        if fuel == 0 {
            return Err(SyncError::ReorgTooDeep { depth });
        }
        fuel -= 1;

        // Lean: let tip := fetchRemote h
        let block = fetcher
            .fetch_at_height(h)
            .await
            .map_err(|e| SyncError::Fetch(e.to_string()))?;

        // Lean: let acc' := tip :: acc
        fragment.push_front(block.clone());

        let cs = state.chain_start();

        // Lean: if h = cs then
        if h == cs {
            // Lean: if cs = 0 then … else …
            if cs == 0 {
                // Lean: if tip.prevHash ≠ genesisHash then .error .chainIncoherent
                if block.prev_hash != crate::types::GENESIS_HASH {
                    return Err(SyncError::ChainIncoherent {
                        height: 0,
                        expected: [0u8; 32],
                        got: block.prev_hash,
                    });
                }
            } else {
                // Lean: match freezer.get? (cs - 1) with
                let expected = state
                    .get_block_by_height(cs - 1)
                    // Lean: | none => .error .chainIncoherent
                    .ok_or(SyncError::ChainIncoherent {
                        height: cs - 1,
                        expected: block.prev_hash,
                        got: [0u8; 32],
                    })?;
                // Lean: if tip.prevHash ≠ fb.hash then .error .fuelExhausted
                if block.prev_hash != expected.hash {
                    return Err(SyncError::ReorgTooDeep { depth });
                }
            }
            // Lean: .ok (cs, acc')
            return Ok((cs, fragment));
        }

        // Lean: h > cs — look up the local block at height h-1 in chain.
        // Chain::get takes the actual height and converts internally via
        // idx = height - chain.start (equivalent to Lean's h - cs - 1).
        let chain = state.chain.read().unwrap();

        // Lean: match chain.get? chainIdx with
        match chain.get(h - 1) {
            // Lean: | none => findTrimIndexInt (h - 1) cs … fuel' …
            None => {}
            // Lean: | some cb => if tip.prevHash = cb.hash then .ok (h, acc')
            Some(cb) => {
                if block.prev_hash == cb.hash {
                    return Ok((h, fragment));
                }
            }
        }

        // Lean: recursive call — keep walking down
        h -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{genesis_block, Block, BlockHash, Height, GENESIS_HASH};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A mock `BlockFetcher` backed by an in-memory height → (hash, block) map.
    struct MockFetcher {
        blocks: Mutex<HashMap<Height, (BlockHash, Block)>>,
        tip_hash: BlockHash,
        tip_height: Height,
    }

    impl MockFetcher {
        fn from_chain(chain: Vec<(BlockHash, Block)>) -> Self {
            let tip_hash = chain.last().expect("chain must not be empty").0;
            let tip_height = chain.last().unwrap().1.height;
            let blocks: HashMap<_, _> = chain
                .into_iter()
                .map(|(h, b)| (b.height, (h, b)))
                .collect();
            MockFetcher {
                blocks: Mutex::new(blocks),
                tip_hash,
                tip_height,
            }
        }
    }

    #[async_trait::async_trait]
    impl BlockFetcher for MockFetcher {
        type Error = String;

        async fn fetch_tip(&self) -> Result<(BlockHash, Height), Self::Error> {
            Ok((self.tip_hash, self.tip_height))
        }

        async fn fetch_batch(
            &mut self,
            from: Height,
            to: Height,
        ) -> Result<Vec<(BlockHash, Block)>, Self::Error> {
            let blocks = self.blocks.lock().unwrap();
            let mut result = Vec::new();
            for h in from..=to {
                if let Some(entry) = blocks.get(&h).cloned() {
                    result.push(entry);
                }
            }
            Ok(result)
        }

        async fn fetch_at_height(
            &mut self,
            height: Height,
        ) -> Result<Block, Self::Error> {
            let blocks = self.blocks.lock().unwrap();
            let (hash, mut block) = blocks
                .get(&height)
                .cloned()
                .ok_or_else(|| format!("no block at height {height}"))?;
            block.hash = hash;
            Ok(block)
        }
    }

    /// Build a simple chain of blocks extending from genesis.
    fn build_chain(
        start_height: Height,
        prev_hash: BlockHash,
        count: u32,
        tag: u8,
    ) -> Vec<(BlockHash, Block)> {
        let mut chain = Vec::new();
        let mut prev = prev_hash;
        for i in 0..count {
            let h = start_height + i;
            let hash = [h as u8, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let mut block = Block::new(h, hash, prev, vec![h as u8]);
            block.hash = hash;
            chain.push((hash, block));
            prev = hash;
        }
        chain
    }

    // =====================================================================
    // find_trim_index tests
    // =====================================================================

    #[tokio::test]
    async fn trim_found_at_local_tip_normal_extension() {
        // Local: genesis + blocks 1..=10
        // Remote extends to height 20 on the same fork (tag=0).
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let local_chain = build_chain(1, GENESIS_HASH, 10, 0);
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip_height(), Some(10));

        let remote_chain = build_chain(11, state.tip(), 10, 0);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        let (trim_from, fragment) = find_trim_index(&state, &mut fetcher, 20, MAX_REORG_DEPTH)
            .await
            .unwrap();
        assert_eq!(trim_from, 11); // fragment[0].height, last kept is 10
        assert_eq!(fragment.len(), 10);
        assert_eq!(fragment[0].height, 11);
    }

    #[tokio::test]
    async fn trim_found_after_shallow_reorg() {
        // Local: genesis + fork-A blocks 1..=10
        // Remote: shares 1..=5, diverges at 6..=12 (tag 1)
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let fork_a = build_chain(1, GENESIS_HASH, 10, 0);
        state.ingest_batch(fork_a.clone()).unwrap();

        let common_hash = fork_a[4].0; // hash at height 5
        let fork_b = build_chain(6, common_hash, 7, 1);
        let mut fetcher = MockFetcher::from_chain(fork_b);

        let (trim_from, fragment) = find_trim_index(&state, &mut fetcher, 12, MAX_REORG_DEPTH)
            .await
            .unwrap();
        assert_eq!(trim_from, 6); // fragment[0].height, last kept is 5
        assert_eq!(fragment.len(), 7);
        assert_eq!(fragment[0].height, 6);
    }

    #[tokio::test]
    async fn trim_not_found_when_fuel_exhausted() {
        // Local: genesis + blocks 1..=10 (tag 0).
        // Remote: completely different (tag 1), fuel=3.
        // Fuel exhausted → ReorgTooDeep error.
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let local_chain = build_chain(1, GENESIS_HASH, 10, 0);
        state.ingest_batch(local_chain).unwrap();

        let remote_chain = build_chain(1, GENESIS_HASH, 20, 1);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        let result = find_trim_index(&state, &mut fetcher, 20, 3).await;

        assert!(matches!(result, Err(SyncError::ReorgTooDeep { depth: 3 })));
    }

    // =====================================================================
    // sync_step integration tests
    // =====================================================================

    #[tokio::test]
    async fn sync_step_normal_extension() {
        // Local: genesis + blocks 1..=5.
        // Remote: blocks 6..=10 on the same fork (tag 0).
        // Height gap = 5 ≤ MAX_REORG_DEPTH → goes straight to short sync.
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let local_chain = build_chain(1, GENESIS_HASH, 5, 0);
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip_height(), Some(5));

        let remote_chain = build_chain(6, state.tip(), 5, 0);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        sync_step(&state, &mut fetcher).await.unwrap();

        assert_eq!(state.tip_height(), Some(10));
        for h in 1..=10 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.height, h);
        }
    }

    #[tokio::test]
    async fn sync_step_reorg_truncate_and_rebuild() {
        // Local: A(1)→B(2)→C(3)→D(4)→E(5) — tag 0.
        // Remote fork diverges at C(3): C→F(4)→G(5)→H(6)→I(7) — tag 1.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let fork_a = build_chain(1, GENESIS_HASH, 5, 0);
        let fork_a_hash3 = fork_a[2].0;
        let fork_a_hash4 = fork_a[3].0;
        let fork_a_hash5 = fork_a[4].0;
        state.ingest_batch(fork_a).unwrap();
        assert_eq!(state.tip_height(), Some(5));

        let fork_b = build_chain(4, fork_a_hash3, 4, 1);
        let fork_b_hash7 = fork_b[3].0;
        let mut fetcher = MockFetcher::from_chain(fork_b);

        sync_step(&state, &mut fetcher).await.unwrap();

        assert_eq!(state.tip(), fork_b_hash7);
        assert_eq!(state.tip_height(), Some(7));

        // Heights 1..=3: original fork-A blocks survive.
        for h in 1..=3 {
            let hash = [
                h as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            assert!(state.get_block_by_hash(&hash).is_some(), "fork-A hash {h} should survive");
        }

        // Old fork-A blocks at heights 4 and 5 are gone.
        assert!(state.get_block_by_hash(&fork_a_hash4).is_none());
        assert!(state.get_block_by_hash(&fork_a_hash5).is_none());

        // Fork-B blocks at heights 4..=7 present.
        for h in 4..=7 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.data, vec![h as u8]);
        }
    }

    #[tokio::test]
    async fn sync_step_deep_divergence_fuel_exhausts_state_untouched() {
        // Local: chain.start=6, blocks 6..=106 (101 blocks, tag 0).
        // Remote: 107 blocks (heights 1..=107, tag 1), completely disjoint.
        // find_trim_index fuel = 101 walks back 101 blocks (107→7) but
        // never finds the fork (all heights have different tag hashes),
        // and never reaches the LMDB boundary (h-1=6 is still in chain).
        // Errors with ReorgTooDeep; state untouched.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing::Level::ERROR)
            .try_init();
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 6).unwrap();
        let local_chain = build_chain(6, GENESIS_HASH, 101, 0);
        let local_tip_hash = local_chain.last().unwrap().0;
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip_height(), Some(106));

        let remote_chain = build_chain(1, GENESIS_HASH, 107, 1);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        let err = sync_step(&state, &mut fetcher).await.unwrap_err();
        assert!(matches!(err, SyncError::ReorgTooDeep { depth: 101 }));

        // State completely untouched.
        assert_eq!(state.tip(), local_tip_hash);
        assert_eq!(state.tip_height(), Some(106));
        for h in 6..=106 {
            let hash = [
                h as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            assert!(state.get_block_by_hash(&hash).is_some(), "local tag-0 block at height {h} should survive");
        }
    }

    // =====================================================================
    // Corner case: forward fill when gap >= D (flush + append_to_freezer)
    // =====================================================================

    #[tokio::test]
    async fn sync_step_forward_fill_large_gap() {
        // Chain starts empty at cs=ct=1000 (simulating post-open with LMDB
        // already holding blocks 0..=999).  Remote tip at 1250.
        // gap = 1250 - 1000 = 250 >= D → flush (no-op, chain empty) +
        // forward fill blocks [1000..1149] to freezer, then slow sync
        // [1150..1250] to chain.  After trim: chain has D blocks (1150..1250).
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 1000).unwrap();

        assert_eq!(state.chain_start(), 1000);
        assert_eq!(state.ct(), 1000);

        let remote_chain = build_chain(1000, GENESIS_HASH, 251, 0);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        sync_step(&state, &mut fetcher).await.unwrap();

        // cs = 1250 - D + 1 = 1150, chain has D blocks [1150..1250].
        assert_eq!(state.chain_start(), 1150);
        assert_eq!(state.ct(), 1251);
        assert_eq!(state.tip_height(), Some(1250));
        assert_eq!(state.ct() - state.chain_start(), 101);

        // Forward-filled blocks persisted in LMDB.
        let lmdb = state.lmdb();
        assert_eq!(lmdb.block_count().unwrap().unwrap(), 1150); // 1000..=1149 = 150 blocks
        assert!(state.get_block_by_height(1000).is_some());
        assert!(state.get_block_by_height(1149).is_some());
    }

    // =====================================================================
    // Corner case: negative gap (remote behind — reorg)
    // =====================================================================

    #[tokio::test]
    async fn sync_step_negative_gap_reorg() {
        // Local chain at cs=10, blocks 10..=110 (101 blocks).  Remote reorged
        // to a shorter fork: shares up to height 50, then diverges with
        // blocks 51..=80 (tag 1).  Remote tip = 80, ct = 111, gap = -31.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 10).unwrap();
        let local_chain = build_chain(10, GENESIS_HASH, 101, 0);
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip_height(), Some(110));
        assert_eq!(state.ct(), 111);

        // Remote: shares genesis + 1..=50 (tag 0), diverges at 51..=80 (tag 1).
        let common_hash = {
            let chain = state.chain.read().unwrap();
            chain.get(50).unwrap().hash
        };
        let fork_b = build_chain(51, common_hash, 30, 1);
        let mut fetcher = MockFetcher::from_chain(fork_b);

        sync_step(&state, &mut fetcher).await.unwrap();

        // After reorg: chain truncated at 50, then fork-B blocks 51..=80
        // appended.  Chain: [10..=50] + [51..=80] = [10..=80] (71 blocks).
        assert_eq!(state.tip_height(), Some(80));
        assert_eq!(state.ct(), 81);

        // Old fork-A blocks above 50 are gone.
        for h in 81..=110 {
            let hash = [
                h as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            assert!(state.get_block_by_hash(&hash).is_none(), "fork-A block at height {h} should be gone");
        }

        // Fork-B blocks present.
        for h in 51..=80 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.height, h);
        }
    }

    // =====================================================================
    // Corner case: already synced (optimisation avoids chain swap)
    // =====================================================================

    #[tokio::test]
    async fn sync_step_already_synced_noop() {
        // Chain has blocks 0..=10.  Remote tip is also at 10, same hash.
        // Early exit catches the match — one RPC call, zero state mutation.
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let local_chain = build_chain(1, GENESIS_HASH, 10, 0);
        let tip_hash = local_chain.last().unwrap().0;
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip(), tip_hash);
        assert_eq!(state.tip_height(), Some(10));

        // Fetcher with identical tip.
        let remote_chain = build_chain(1, GENESIS_HASH, 10, 0);
        let mut fetcher = MockFetcher::from_chain(remote_chain);

        sync_step(&state, &mut fetcher).await.unwrap();

        // State unchanged.
        assert_eq!(state.tip(), tip_hash);
        assert_eq!(state.tip_height(), Some(10));
    }

    // =====================================================================
    // Corner case: chain exceeds D after append, trim brings it back
    // =====================================================================

    #[tokio::test]
    async fn sync_step_chain_exceeds_d_then_trimmed() {
        // Chain has genesis + 101 blocks = 102 blocks (> D).  Remote extends
        // by 30 blocks on same fork: [102..131].  After append + trim: cs
        // advances by 31 and the chain holds exactly D blocks.
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();

        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let local_chain = build_chain(1, GENESIS_HASH, 101, 0);
        let tip_hash = local_chain.last().unwrap().0;
        state.ingest_batch(local_chain).unwrap();
        assert_eq!(state.tip_height(), Some(101));
        assert_eq!(state.ct(), 102);

        // Remote extends by 30 blocks: [102..131].
        let extension = build_chain(102, tip_hash, 30, 0);
        let mut fetcher = MockFetcher::from_chain(extension);

        sync_step(&state, &mut fetcher).await.unwrap();

        // Tip extended.  Trim froze blocks 0..=30 (31 blocks); cs advanced.
        assert_eq!(state.tip_height(), Some(131));
        assert_eq!(state.ct() - state.chain_start(), 101);
        assert_eq!(state.chain_start(), 31);

        // Frozen blocks in LMDB.
        let lmdb = state.lmdb();
        let count = lmdb.block_count().unwrap().unwrap();
        assert_eq!(count, 31); // blocks 0..=30 frozen = 31 blocks
        assert!(state.get_block_by_height(0).is_some());
        assert!(state.get_block_by_height(30).is_some());
    }
}
