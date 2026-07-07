//! Compact block serving backed by the zaino-store block store.
//!
//! Unlike [`NodeBackedChainIndexSubscriber`], the block store is hash-keyed
//! and append-only — no snapshot ceremony needed. The [`CompactBlockPublisher`]
//! trait methods are implemented directly against [`ChainState`].
//!
//! The sync loop lives in [`zaino_store::sync`]; this module provides the
//! Zcash-specific [`BlockFetcher`] implementation that bridges the store's
//! generic sync algorithm to the [`BlockchainSource`] validator interface.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use primitive_types::U256;
use tokio::sync::{mpsc, Semaphore};
use tonic::Status;
use tracing::{info, warn};
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};
use zaino_store::fetcher::BlockFetcher;
use zaino_store::{Block, ChainState};
use zebra_chain::parameters::NetworkUpgrade;

use crate::chain_index::source::BlockchainSource;
use crate::chain_index::types::helpers::{BlockMetadata, BlockWithMetadata};
use crate::chain_index::types::{BlockHash, Height};
use crate::chain_index::{ChainIndexBase, CompactBlockPublisher, NonFinalizedSnapshot};
use crate::error::ChainIndexError;
use crate::{ChainWork, CompactBlockStream, IndexedBlock};

// =========================================================================
// No-op snapshot for trait compatibility
// =========================================================================

/// A no-op snapshot for the block store. The block store never uses snapshots
/// (it is hash-keyed and append-only), but [`CompactBlockPublisher`] inherits
/// `type Snapshot` from [`ChainIndexBase`]. This type satisfies the bound
/// without adding any runtime overhead.
#[derive(Debug, Clone)]
pub struct BlockStoreSnapshot;

impl NonFinalizedSnapshot for BlockStoreSnapshot {
    fn get_chainblock_by_hash(&self, _target_hash: &BlockHash) -> Option<&IndexedBlock> {
        None
    }

    fn get_chainblock_by_height(&self, _target_height: &Height) -> Option<&IndexedBlock> {
        None
    }

    fn max_serviceable_height(&self) -> &Height {
        // Never used in practice — the block store doesn't call this.
        static ZERO: Height = Height(0);
        &ZERO
    }
}

impl ChainIndexBase for ChainState {
    type Snapshot = BlockStoreSnapshot;
    type Error = ChainIndexError;
}

// =========================================================================
// CompactBlockPublisher — thin serving layer on top of ChainState
// =========================================================================

impl CompactBlockPublisher for ChainState {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlock>, ChainIndexError> {
        let Some(block) = self.get_block_by_height(height.0) else {
            return Ok(None);
        };

        let compact = compact_block_from_stored(&block, &pool_types.to_pool_types_vector())
            .ok_or_else(|| {
                ChainIndexError::backing_validator(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to decode stored compact block",
                ))
            })?;

        Ok(Some(compact))
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, ChainIndexError> {
        let tip_height = self.tip_height().unwrap_or(0);

        if start_height.0 > tip_height && end_height.0 > tip_height {
            return Ok(None);
        }

        let capped_end = u32::min(end_height.0, tip_height);
        let mut iter = self.stream_blocks(start_height.0, capped_end);

        let pool_types_vector = pool_types.to_pool_types_vector();
        let (tx, rx) = mpsc::channel::<Result<CompactBlock, Status>>(128);

        tokio::spawn(async move {
            loop {
                match iter.next() {
                    Some(Ok(block)) => {
                        if let Some(compact) =
                            compact_block_from_stored(&block, &pool_types_vector)
                        {
                            if tx.send(Ok(compact)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "block_iter error");
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "block store: stream error: {e}"
                            ))))
                            .await;
                        return;
                    }
                    None => break,
                }
            }
            info!("block_iter stream exhausted");
        });

        Ok(Some(CompactBlockStream::new(rx)))
    }
}

/// Deserialize a stored compact block protobuf and apply pool-type filtering.
fn compact_block_from_stored(
    stored: &Block,
    pool_types_vector: &[zaino_proto::proto::service::PoolType],
) -> Option<CompactBlock> {
    let full: CompactBlock = prost::Message::decode(&*stored.data).ok()?;
    Some(compact_block_with_pool_types(full, pool_types_vector))
}

// =========================================================================
// BlockFetcher — Zcash-specific fetch+build impl
// =========================================================================

/// Maximum concurrent `get_block` + `get_commitment_tree_roots` calls
/// to the source node.
const DEFAULT_MAX_CONCURRENCY: usize = 8;

/// Number of batches to accumulate before making an adaptive-concurrency
/// decision.
const WINDOW_SIZE: u32 = 2;

/// Throughput improvement threshold — fraction above 1.0 that counts as
/// a genuine improvement.
const IMPROVE_THRESHOLD: f64 = 1.05;

/// Throughput degradation threshold — fraction below 1.0 that counts as
/// a genuine degradation.
const DEGRADE_THRESHOLD: f64 = 0.95;

// =========================================================================
// Adaptive concurrency controller
// =========================================================================

/// Hill-climbing controller that adjusts the fetch concurrency to maximise
/// throughput (bytes/sec), measured over windows of [`WINDOW_SIZE`] batches.
///
/// Starts at `max_concurrency`, gathers a baseline window, then probes
/// downward. When throughput is flat it drifts toward lower concurrency
/// (same throughput, less memory). When throughput degrades it reverses
/// direction. The result is the lowest concurrency that still delivers
/// peak throughput.
#[derive(Clone)]
struct AdaptiveConcurrency {
    /// Hard ceiling from config.
    max: usize,
    /// Effective concurrency for the current window.
    current: usize,
    /// Current probe direction: `1` = Up (more concurrency), `-1` = Down.
    direction: i8,
    /// Average throughput of the previous window. `None` until the
    /// bootstrap window completes.
    prev_avg_tput: Option<f64>,
    /// Running sum of throughput samples in the current window.
    window_sum: f64,
    /// Number of samples accumulated in the current window.
    window_count: u32,
}

impl AdaptiveConcurrency {
    fn new(max: usize) -> Self {
        let start = usize::max(max / 2, 1);
        Self {
            max,
            current: start,
            direction: 1, // Up
            prev_avg_tput: None,
            window_sum: 0.0,
            window_count: 0,
        }
    }

    /// Returns the concurrency to use for the next batch.
    fn current(&self) -> usize {
        self.current
    }

    /// Record one batch's throughput measurement. If the window fills,
    /// make an adaptation decision and reset.
    fn record(&mut self, throughput: f64) {
        self.window_sum += throughput;
        self.window_count += 1;

        info!(
            sample = self.window_count,
            window_size = WINDOW_SIZE,
            throughput_bytes_per_sec = throughput,
            "adaptive window sample"
        );

        if self.window_count < WINDOW_SIZE {
            return;
        }

        let avg_tput = self.window_sum / self.window_count as f64;
        info!(
            sum = self.window_sum,
            count = self.window_count,
            avg_tput_bytes_per_sec = avg_tput,
            "adaptive window full"
        );
        self.window_sum = 0.0;
        self.window_count = 0;

        if let Some(prev_avg) = self.prev_avg_tput {
            let ratio = if prev_avg > 0.0 {
                avg_tput / prev_avg
            } else {
                1.0
            };

            let old_concurrency = self.current;
            let new_concurrency: usize;
            let reason: &str;

            if ratio < DEGRADE_THRESHOLD {
                reason = "degrade";
                new_concurrency = (self.current as isize - self.direction as isize)
                    .clamp(1, self.max as isize) as usize;
                self.direction = -self.direction;
            } else {
                // Improve or flat — keep going in the same direction.
                reason = if ratio > IMPROVE_THRESHOLD { "improve" } else { "flat" };
                new_concurrency = (self.current as isize + self.direction as isize)
                    .clamp(1, self.max as isize) as usize;
            }

            info!(
                reason,
                direction = self.direction,
                old = old_concurrency,
                new = new_concurrency,
                avg_tput_bytes_per_sec = avg_tput,
                prev_avg_tput_bytes_per_sec = prev_avg,
                ratio,
                "adaptive window decision"
            );

            self.current = new_concurrency;
            self.prev_avg_tput = Some(avg_tput);
        } else {
            // Bootstrap complete — first window establishes the baseline.
            // Probe downward for the next window.
            let old = self.current;
            let new = (old as isize + self.direction as isize)
                .clamp(1, self.max as isize) as usize;
            self.current = new;
            info!(
                old,
                new,
                direction = self.direction,
                bootstrap_avg_tput_bytes_per_sec = avg_tput,
                "adaptive concurrency bootstrap complete"
            );
            self.prev_avg_tput = Some(avg_tput);
        }
    }
}

/// A [`BlockFetcher`] that fetches from a [`BlockchainSource`] and builds
/// Zcash blocks (zebra block → IndexedBlock → CompactBlock protobuf → store
/// bytes).
pub struct ChainSourceFetcher<S: BlockchainSource> {
    source: S,
    network: zebra_chain::parameters::Network,
    max_concurrency: usize,
    adaptive: AdaptiveConcurrency,
}

impl<S: BlockchainSource> ChainSourceFetcher<S> {
    /// Create a new fetcher wrapping a [`BlockchainSource`].
    pub fn new(source: S, network: zebra_chain::parameters::Network) -> Self {
        Self {
            source,
            network,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            adaptive: AdaptiveConcurrency::new(DEFAULT_MAX_CONCURRENCY),
        }
    }

    /// Override the max concurrency for block fetching.
    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self.adaptive = AdaptiveConcurrency::new(max_concurrency);
        self
    }
}

impl<S: BlockchainSource> Clone for ChainSourceFetcher<S> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            network: self.network.clone(),
            max_concurrency: self.max_concurrency,
            adaptive: self.adaptive.clone(),
        }
    }
}

#[async_trait]
impl<S: BlockchainSource + Clone + Send + Sync + 'static> BlockFetcher for ChainSourceFetcher<S> {
    type Error = String;

    async fn fetch_tip(&self) -> Result<([u8; 32], u32), Self::Error> {
        let remote_hash = self
            .source
            .get_best_block_hash()
            .await
            .map_err(|e| format!("get_best_block_hash: {e}"))?
            .ok_or_else(|| "validator returned no best block hash".to_string())?;

        let remote_block = self
            .source
            .get_block(zebra_state::HashOrHeight::Hash(remote_hash))
            .await
            .map_err(|e| format!("get_block(hash): {e}"))?
            .ok_or_else(|| format!("validator missing block for tip {remote_hash:?}"))?;

        let remote_height = remote_block
            .coinbase_height()
            .ok_or_else(|| "validator returned tip block without height".to_string())?
            .0;

        Ok((remote_hash.0, remote_height))
    }

    async fn fetch_batch(
        &mut self,
        from: u32,
        to: u32,
    ) -> Result<Vec<([u8; 32], Block)>, Self::Error> {
        let fetch_start = Instant::now();
        let sapling_activation_height = NetworkUpgrade::Sapling
            .activation_height(&self.network)
            .expect("Sapling activation height must be set")
            .0;
        let nu5_activation_height = NetworkUpgrade::Nu5.activation_height(&self.network);

        let effective_concurrency = self.adaptive.current();
        info!(
            concurrency = effective_concurrency,
            from,
            to,
            "fetching batch"
        );
        let semaphore = Arc::new(Semaphore::new(effective_concurrency));
        let mut fetch_tasks = Vec::new();

        for h in from..=to {
            let source = self.source.clone();
            let sem = Arc::clone(&semaphore);

            fetch_tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;

                let zebra_block = source
                    .get_block(zebra_state::HashOrHeight::Height(
                        zebra_chain::block::Height(h),
                    ))
                    .await
                    .map_err(|e| format!("get_block({h}): {e}"))?
                    .ok_or_else(|| format!("validator missing block at height {h}"))?;

                let block_hash = BlockHash(zebra_block.hash().0);
                let tree_roots = source
                    .get_commitment_tree_roots(block_hash)
                    .await
                    .map_err(|e| format!("get_commitment_tree_roots({h}): {e}"))?;

                let is_sapling_active = h >= sapling_activation_height;
                let is_orchard_active =
                    nu5_activation_height.is_some_and(|nu5| h >= nu5.0);

                let (sapling_root, sapling_size) = if is_sapling_active {
                    tree_roots.0.unwrap_or_default()
                } else {
                    (zebra_chain::sapling::tree::Root::default(), 0)
                };
                let (orchard_root, orchard_size) = if is_orchard_active {
                    tree_roots.1.unwrap_or_default()
                } else {
                    (zebra_chain::orchard::tree::Root::default(), 0)
                };

                Ok::<_, String>((
                    h,
                    zebra_block,
                    sapling_root,
                    sapling_size,
                    orchard_root,
                    orchard_size,
                ))
            }));
        }

        let fetched = futures::future::try_join_all(fetch_tasks)
            .await
            .map_err(|e| format!("fetch task panicked: {e}"))?;

        let mut fetched: Vec<_> = fetched.into_iter().collect::<Result<Vec<_>, _>>()?;
        fetched.sort_by_key(|(h, ..)| *h);

        // Build IndexedBlock → CompactBlock protobuf → store bytes.
        let mut parent_chainwork = ChainWork::from_u256(U256::zero());
        let mut batch = Vec::new();

        for (h, zebra_block, sapling_root, sapling_size, orchard_root, orchard_size) in fetched {
            let block_hash = zebra_block.hash();
            let prev_hash = zebra_block.header.previous_block_hash;

            let metadata = BlockMetadata::new(
                sapling_root,
                sapling_size as u32,
                orchard_root,
                orchard_size as u32,
                parent_chainwork,
                self.network.clone(),
            );
            let indexed_block =
                IndexedBlock::try_from(BlockWithMetadata::new(&zebra_block, metadata))
                    .map_err(|e| format!("IndexedBlock::try_from at height {h}: {e}"))?;

            parent_chainwork = indexed_block.context.chainwork;

            let compact_block = indexed_block.to_compact_block();
            let compact_bytes = prost::Message::encode_to_vec(&compact_block);

            batch.push((
                block_hash.0,
                Block::new(h, block_hash.0, prev_hash.0, compact_bytes),
            ));
        }

        // Record throughput for adaptive concurrency.
        let total_bytes: usize = batch.iter().map(|(_, b)| b.data.len()).sum();
        let elapsed_secs = fetch_start.elapsed().as_secs_f64();
        if elapsed_secs > 0.0 {
            self.adaptive.record(total_bytes as f64 / elapsed_secs);
        }

        Ok(batch)
    }

    async fn fetch_at_height(
        &mut self,
        height: u32,
    ) -> Result<Block, Self::Error> {
        let sapling_activation_height = NetworkUpgrade::Sapling
            .activation_height(&self.network)
            .expect("Sapling activation height must be set")
            .0;
        let nu5_activation_height = NetworkUpgrade::Nu5.activation_height(&self.network);

        let zebra_block = self
            .source
            .get_block(zebra_state::HashOrHeight::Height(
                zebra_chain::block::Height(height),
            ))
            .await
            .map_err(|e| format!("get_block({height}): {e}"))?
            .ok_or_else(|| format!("validator missing block at height {height}"))?;

        let block_hash = BlockHash(zebra_block.hash().0);
        let prev_hash = zebra_block.header.previous_block_hash;
        let parent_chainwork = ChainWork::from_u256(U256::zero()); // no chainwork for single fetch

        let is_sapling_active = height >= sapling_activation_height;
        let is_orchard_active =
            nu5_activation_height.is_some_and(|nu5| height >= nu5.0);

        let tree_roots = self
            .source
            .get_commitment_tree_roots(block_hash)
            .await
            .map_err(|e| format!("get_commitment_tree_roots({height}): {e}"))?;

        let (sapling_root, sapling_size) = if is_sapling_active {
            tree_roots.0.unwrap_or_default()
        } else {
            (zebra_chain::sapling::tree::Root::default(), 0)
        };
        let (orchard_root, orchard_size) = if is_orchard_active {
            tree_roots.1.unwrap_or_default()
        } else {
            (zebra_chain::orchard::tree::Root::default(), 0)
        };

        let metadata = BlockMetadata::new(
            sapling_root,
            sapling_size as u32,
            orchard_root,
            orchard_size as u32,
            parent_chainwork,
            self.network.clone(),
        );
        let indexed_block =
            IndexedBlock::try_from(BlockWithMetadata::new(&zebra_block, metadata))
                .map_err(|e| format!("IndexedBlock::try_from at height {height}: {e}"))?;

        let compact_block = indexed_block.to_compact_block();
        let compact_bytes = prost::Message::encode_to_vec(&compact_block);

        Ok(Block::new(height, block_hash.0, prev_hash.0, compact_bytes))
    }
}

// =========================================================================
// AdaptiveConcurrency tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `n` samples of `throughput` into the controller.
    fn feed_samples(ac: &mut AdaptiveConcurrency, throughput: f64, n: u32) {
        for _ in 0..n {
            ac.record(throughput);
        }
    }

    #[test]
    fn bootstrap_first_window_sets_baseline_and_probes() {
        let mut ac = AdaptiveConcurrency::new(8);
        // Starts at max/2 = 4.
        assert_eq!(ac.current(), 4);

        // First window (2 batches at 100 KB/s).
        feed_samples(&mut ac, 100.0, 2);
        // Bootstrap: direction Up, so probe from 4 to 5 and set the baseline.
        assert_eq!(ac.current(), 5);
        assert!(ac.prev_avg_tput.is_some());
    }

    #[test]
    fn improve_keeps_going_in_same_direction() {
        let mut ac = AdaptiveConcurrency::new(8);

        // Bootstrap: starts at 4, direction Up → probe to 5. Baseline = 100.
        feed_samples(&mut ac, 100.0, 2);
        assert_eq!(ac.current(), 5); // bootstrapped, direction = 1 (Up)

        // Second window: 20% better throughput at concurrency 5.
        // ratio = 120 / 100 = 1.20 > 1.05
        feed_samples(&mut ac, 120.0, 2);
        // Direction was Up, improvement → keep going up.
        assert_eq!(ac.current(), 6);
    }

    #[test]
    fn degrade_undoes_and_reverses_direction() {
        // Set up with state already past bootstrap.
        let mut ac = AdaptiveConcurrency::new(4);
        // Manually set state to skip bootstrap.
        ac.current = 3;
        ac.direction = 1; // Up
        ac.prev_avg_tput = Some(80.0);

        // Window: throughput = 60, ratio = 60/80 = 0.75 < 0.95
        feed_samples(&mut ac, 60.0, 2);
        // Degrade: undo Up direction → go Down (3 - 1 = 2), reverse direction to -1.
        assert_eq!(ac.current(), 2);
        assert_eq!(ac.direction, -1);
    }

    #[test]
    fn flat_drifts_toward_efficiency() {
        let mut ac = AdaptiveConcurrency::new(8);
        // Bootstrap baseline = 100 (starts at 4, direction Up → probes to 5).
        feed_samples(&mut ac, 100.0, 2);
        assert_eq!(ac.current(), 5);
        assert!(ac.prev_avg_tput.is_some());

        // Manually set direction to Up so we can test flat-after-going-up.
        ac.direction = 1;
        ac.current = 5;
        ac.prev_avg_tput = Some(100.0);

        // Window: throughput = 103, ratio = 1.03 — within noise band (0.95–1.05).
        feed_samples(&mut ac, 103.0, 2);
        // Flat: keep going same direction (Up) → 5+1 = 6.
        assert_eq!(ac.current(), 6);
        assert_eq!(ac.direction, 1);
    }

    #[test]
    fn concurrency_clamped_to_one() {
        let mut ac = AdaptiveConcurrency::new(4);
        // Set up so flat drift would go below 1.
        ac.current = 1;
        ac.direction = -1;
        ac.prev_avg_tput = Some(100.0);

        // Flat → try to drop, but clamped at 1.
        feed_samples(&mut ac, 103.0, 2);
        assert_eq!(ac.current(), 1);
    }

    #[test]
    fn concurrency_clamped_to_max() {
        let mut ac = AdaptiveConcurrency::new(4);
        // Manually set state to just below max, direction Up.
        ac.current = 3;
        ac.direction = 1;
        ac.prev_avg_tput = Some(10.0);

        // Huge improvement → keep going up, but clamped at max=4.
        feed_samples(&mut ac, 1000.0, 2);
        assert_eq!(ac.current(), 4); // clamped at max
    }

    #[test]
    fn partial_window_no_decision() {
        let mut ac = AdaptiveConcurrency::new(8);
        // Feed 1 sample (less than WINDOW_SIZE=2).
        feed_samples(&mut ac, 100.0, 1);
        // No decision yet — still at start (max/2 = 4).
        assert_eq!(ac.current(), 4);
        // prev_avg_tput still None (bootstrap not yet triggered).
        assert!(ac.prev_avg_tput.is_none());
        // Window should have 1 sample accumulated.
        assert_eq!(ac.window_count, 1);
        assert!((ac.window_sum - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_prev_avg_handled_gracefully() {
        let mut ac = AdaptiveConcurrency::new(8);
        // Bootstrap: baseline.
        feed_samples(&mut ac, 100.0, 2);
        // prev_avg is now set to 100.0

        // Manually set prev_avg to 0 to test the zero-guard.
        ac.prev_avg_tput = Some(0.0);
        ac.current = 4;
        ac.direction = 1;

        // ratio = 50 / 0 → guarded to 1.0 → flat → keep going Up → 4+1=5.
        feed_samples(&mut ac, 50.0, 2);
        assert_eq!(ac.current(), 5);
    }
}
