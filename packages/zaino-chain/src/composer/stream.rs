//! Walking a height range across providers, a chunk at a time.
//!
//! # Chunks, and why they are sized by bytes
//!
//! A range read yields `Vec`s rather than single blocks: per-item overhead over
//! hundreds of thousands of blocks dominates, and the store's own range ports
//! are chunked for the same reason.
//!
//! Chunk *size* is a byte budget rather than a block count because block sizes
//! vary by orders of magnitude across the chain — early regtest blocks are a
//! few hundred bytes, busy mainnet blocks are megabytes. A fixed count would
//! make memory per client swing with block size, and with thousands of clients
//! that is the difference between a predictable footprint and an unpredictable
//! one. The walk starts small so latency to first byte stays low, then adapts
//! toward the budget from what it has actually seen.
//!
//! # Lazy, not buffered
//!
//! The walk produces only when polled, so a slow client applies backpressure by
//! not polling. A producer task writing into a channel would instead buffer per
//! client, which at this scale is the difference between bounded and unbounded
//! memory.

use zaino_chain_store::PoolFilter;
use zaino_primitives::types::{
    ChainMetadata, CompactBlock, Height, PreIndexCompactBlock, PreIndexCompactTx, ShieldedPool,
    TreeRoots, TreeSize,
};

use crate::composer::coverage::Segment;

/// How many blocks to ask for next.
///
/// Adaptive: the first chunk is deliberately small, and later ones are sized
/// from the average block size observed so far.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Chunker {
    budget_bytes: usize,
    count: u32,
}

impl Chunker {
    /// Blocks in the first chunk.
    ///
    /// Small on purpose. A wallet's first chunk arriving quickly matters more
    /// than it being full, and until a block has been seen there is nothing to
    /// size a budget against.
    const FIRST: u32 = 16;

    /// The most blocks in any one chunk.
    ///
    /// A ceiling for the degenerate case of near-empty blocks, where the byte
    /// budget alone would ask for an unbounded run.
    const MAX: u32 = 2_000;

    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            count: Self::FIRST,
        }
    }

    /// How many blocks the next chunk should cover.
    pub(crate) fn count(&self) -> u32 {
        self.count.max(1)
    }

    /// Adapts from a chunk that was actually produced.
    pub(crate) fn observe(&mut self, blocks: usize, bytes: usize) {
        if blocks == 0 {
            return;
        }
        let average = (bytes / blocks).max(1);
        let target = (self.budget_bytes / average).clamp(1, Self::MAX as usize);
        self.count = target as u32;
    }
}

/// The heights a segment covers, from `start`, at most `count` of them.
///
/// Returns the run and what remains of the segment, so a walk can consume a
/// segment across several chunks without re-planning.
pub(crate) fn take(segment: Segment, count: u32) -> (Vec<Height>, Option<Segment>) {
    let start = u32::from(segment.start);
    let end = u32::from(segment.end);
    let last = start.saturating_add(count.saturating_sub(1)).min(end);

    let heights = (start..=last)
        .filter_map(|height| Height::try_from(height).ok())
        .collect();

    let remainder = last
        .checked_add(1)
        .filter(|next| *next <= end)
        .and_then(|next| Height::try_from(next).ok())
        .map(|next| Segment {
            provider: segment.provider,
            start: next,
            end: segment.end,
        });

    (heights, remainder)
}

/// A rough serialized size, for chunk sizing only.
///
/// Never a wire size and never used as one: it exists so the walk can adapt its
/// chunk length, where being within a factor of two is ample. Computing an
/// exact encoding per block would cost more than the sizing saves.
pub(crate) fn approx_size(block: &CompactBlock) -> usize {
    /// Header, hashes, and the fixed metadata fields.
    const OVERHEAD: usize = 128;
    /// A txid plus its framing.
    const PER_TX: usize = 40;
    /// A nullifier, or an output's cmu + epk + ciphertext head.
    const PER_NOTE: usize = 116;
    /// An outpoint or a script-bearing output.
    const PER_TRANSPARENT: usize = 40;

    OVERHEAD
        + block
            .transactions
            .iter()
            .map(|tx| {
                PER_TX
                    + PER_NOTE
                        * (tx.sapling_nullifiers.len()
                            + tx.sapling_outputs.len()
                            + tx.orchard_actions.len()
                            + tx.ironwood_actions.len())
                    + PER_TRANSPARENT * (tx.transparent_inputs.len() + tx.transparent_outputs.len())
            })
            .sum::<usize>()
}

/// The compact block a caller asked for, from a pre-index projection.
///
/// Shared by the chain head and the validator so a compact block above the
/// store's range has one definition however it was obtained.
///
/// # The filter is applied here, not skipped
///
/// The store pushes [`PoolFilter`] into its read, where it decides which row
/// families decode at all. There is nothing to push it into for an
/// already-projected block, so above the store it is applied afterwards. That
/// is a performance difference and must not become a semantic one: a caller
/// filtering to sapling gets the same shape from every provider, including the
/// same transactions omitted.
pub(crate) fn compact_from_pre_index(
    pre_index: PreIndexCompactBlock,
    chain_metadata: ChainMetadata,
    pools: PoolFilter,
) -> CompactBlock {
    let transactions = pre_index
        .transactions
        .into_iter()
        .map(|tx| filter_tx(tx, pools))
        .filter(retains_anything)
        .collect();

    CompactBlock {
        hash: pre_index.hash,
        prev_hash: pre_index.prev_hash,
        height: pre_index.height,
        time: pre_index.time,
        bits: pre_index.bits,
        transactions,
        chain_metadata,
    }
}

/// The cumulative tree sizes a compact block reports, from commitment roots.
pub(crate) fn metadata_from_roots(roots: &TreeRoots) -> ChainMetadata {
    ChainMetadata {
        sapling_tree_size: size_of(roots.sapling.as_ref()),
        orchard_tree_size: size_of(roots.orchard.as_ref()),
        ironwood_tree_size: size_of(roots.ironwood.as_ref()),
    }
}

fn size_of(info: Option<&zaino_primitives::types::TreeRootInfo>) -> TreeSize {
    info.map_or(TreeSize::ZERO, |info| info.size)
}

/// Clears the pools the caller did not ask for.
fn filter_tx(mut tx: PreIndexCompactTx, pools: PoolFilter) -> PreIndexCompactTx {
    if !pools.includes_transparent() {
        tx.transparent_inputs.clear();
        tx.transparent_outputs.clear();
    }
    if !pools.includes(ShieldedPool::Sapling) {
        tx.sapling_nullifiers.clear();
        tx.sapling_outputs.clear();
    }
    if !pools.includes(ShieldedPool::Orchard) {
        tx.orchard_actions.clear();
    }
    if !pools.includes(ShieldedPool::Ironwood) {
        tx.ironwood_actions.clear();
    }
    tx
}

/// Whether a filtered transaction still carries anything asked for.
fn retains_anything(tx: &PreIndexCompactTx) -> bool {
    !tx.transparent_inputs.is_empty()
        || !tx.transparent_outputs.is_empty()
        || !tx.sapling_nullifiers.is_empty()
        || !tx.sapling_outputs.is_empty()
        || !tx.orchard_actions.is_empty()
        || !tx.ironwood_actions.is_empty()
}

// ***** The walk *****

use futures::stream::Stream;
use std::future::Future;
use std::pin::Pin;

use crate::composer::coverage::Coverage;
use crate::error::{ChainViewError, Result};

/// A boxed fetch of one chunk from one provider.
///
/// Boxed because the closure's future is named nowhere: the walk holds a
/// closure returning a future whose type depends on the snapshot's three
/// parameters, and an unboxed one cannot be stored in the state a `Stream`
/// carries. One allocation per chunk, against a batch of blocks it is about to
/// fetch from disk or over a network.
pub(crate) type ChunkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Walks `start..=end` across providers, yielding a chunk at a time.
///
/// The chunk length adapts by bytes; `size_of` measures a produced chunk so the
/// next one can be sized from what the chain actually holds there.
pub(crate) fn walk_sized<S, T, F, M>(
    snapshot: S,
    start: Height,
    end: Height,
    fetch: F,
    size_of: M,
) -> impl Stream<Item = Result<Vec<T>>> + Send
where
    S: Coverable + Clone + Send + Sync + 'static,
    T: Send + 'static,
    F: for<'a> Fn(&'a S, Segment, Vec<Height>) -> ChunkFuture<'a, Vec<T>> + Send + Sync + 'static,
    M: Fn(&[T]) -> usize + Send + Sync + 'static,
{
    let budget = snapshot.chunk_budget_bytes();
    let plan = snapshot.coverage().segments(start, end);

    futures::stream::unfold(
        Walk {
            snapshot,
            state: match plan {
                // Planning fails only where a hole cannot be filled. Carried as
                // the stream's first and only item so a caller sees the same
                // error whether it asked for one block or a million.
                Err(()) => WalkState::Failed,
                Ok(segments) => WalkState::Running {
                    segments: segments.into_iter(),
                    current: None,
                    chunker: Chunker::new(budget),
                },
            },
            fetch,
            size_of,
        },
        |mut walk| async move {
            let item = walk.next().await?;
            Some((item, walk))
        },
    )
}

/// [`walk_sized`] for chunks whose size is not worth measuring.
///
/// Indexed and raw blocks are read far less often than compact ones and by
/// callers that are not streaming the whole chain, so they use a fixed chunk
/// rather than carrying a size estimator for each payload type.
pub(crate) fn walk<S, T, F>(
    snapshot: S,
    start: Height,
    end: Height,
    fetch: F,
) -> impl Stream<Item = Result<Vec<T>>> + Send
where
    S: Coverable + Clone + Send + Sync + 'static,
    T: Send + 'static,
    F: for<'a> Fn(&'a S, Segment, Vec<Height>) -> ChunkFuture<'a, Vec<T>> + Send + Sync + 'static,
{
    walk_sized(snapshot, start, end, fetch, |_| 0)
}

/// What the walk needs from a snapshot.
///
/// A trait rather than a concrete type so this module does not need the
/// snapshot's three parameters, and so it can be exercised on its own.
pub(crate) trait Coverable {
    /// What the providers cover.
    fn coverage(&self) -> Coverage;
    /// The byte budget a chunk aims for.
    fn chunk_budget_bytes(&self) -> usize;
}

enum WalkState {
    Running {
        segments: std::vec::IntoIter<Segment>,
        current: Option<Segment>,
        chunker: Chunker,
    },
    /// The range could not be planned; yield one error and stop.
    Failed,
    Done,
}

struct Walk<S, F, M> {
    snapshot: S,
    state: WalkState,
    fetch: F,
    size_of: M,
}

impl<S, T, F, M> Walk<S, F, M>
where
    S: Coverable,
    F: for<'a> Fn(&'a S, Segment, Vec<Height>) -> ChunkFuture<'a, Vec<T>>,
    M: Fn(&[T]) -> usize,
{
    async fn next(&mut self) -> Option<Result<Vec<T>>> {
        loop {
            match &mut self.state {
                WalkState::Done => return None,
                WalkState::Failed => {
                    self.state = WalkState::Done;
                    return Some(Err(ChainViewError::NotServiceable(
                        "no provider covers part of this range and the validator is disabled",
                    )));
                }
                WalkState::Running {
                    segments,
                    current,
                    chunker,
                } => {
                    let Some(segment) = current.take().or_else(|| segments.next()) else {
                        self.state = WalkState::Done;
                        return None;
                    };

                    let (heights, remainder) = take(segment, chunker.count());
                    *current = remainder;
                    if heights.is_empty() {
                        continue;
                    }

                    let asked = heights.len();
                    let chunk = match (self.fetch)(&self.snapshot, segment, heights).await {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            self.state = WalkState::Done;
                            return Some(Err(error));
                        }
                    };

                    // Short of what was asked for means a provider ran out
                    // mid-range. Stop rather than skipping ahead: a caller
                    // walking heights would read a gap as "these blocks are
                    // empty", and the run must stay contiguous.
                    let short = chunk.len() < asked;

                    let bytes = (self.size_of)(&chunk);
                    chunker.observe(chunk.len(), bytes);

                    if short {
                        self.state = WalkState::Done;
                    }
                    if chunk.is_empty() {
                        self.state = WalkState::Done;
                        return None;
                    }
                    return Some(Ok(chunk));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::coverage::Provider;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height is within the protocol limit")
    }

    fn segment(start: u32, end: u32) -> Segment {
        Segment {
            provider: Provider::Store,
            start: height(start),
            end: height(end),
        }
    }

    /// A chunk smaller than the segment leaves the rest behind.
    #[test]
    fn take_splits_a_segment() {
        let (heights, rest) = take(segment(10, 20), 4);
        assert_eq!(heights.first().copied(), Some(height(10)));
        assert_eq!(heights.last().copied(), Some(height(13)));
        assert_eq!(heights.len(), 4);
        assert_eq!(rest, Some(segment(14, 20)));
    }

    /// A chunk covering the segment leaves nothing.
    #[test]
    fn take_consumes_a_segment() {
        let (heights, rest) = take(segment(10, 12), 100);
        assert_eq!(heights.len(), 3);
        assert_eq!(rest, None);
    }

    /// Every height is yielded exactly once across repeated takes.
    ///
    /// The invariant a streamed range depends on: a dropped height is a missing
    /// block and a repeated one is a duplicate, and neither is visible to a
    /// caller concatenating chunks.
    #[test]
    fn repeated_takes_tile_the_segment() {
        let mut current = Some(segment(0, 99));
        let mut seen = Vec::new();
        while let Some(segment) = current {
            let (heights, rest) = take(segment, 7);
            seen.extend(heights);
            current = rest;
        }
        assert_eq!(seen.len(), 100);
        assert_eq!(seen.first().copied(), Some(height(0)));
        assert_eq!(seen.last().copied(), Some(height(99)));
        for pair in seen.windows(2) {
            assert_eq!(u32::from(pair[0]) + 1, u32::from(pair[1]));
        }
    }

    /// The first chunk is small, and later ones adapt to the byte budget.
    #[test]
    fn the_chunker_ramps_toward_its_budget() {
        let mut chunker = Chunker::new(1024 * 1024);
        assert_eq!(chunker.count(), Chunker::FIRST);

        // 1 KiB blocks: the budget allows about a thousand.
        chunker.observe(16, 16 * 1024);
        assert_eq!(chunker.count(), 1024);

        // 1 MiB blocks: the budget allows one.
        chunker.observe(4, 4 * 1024 * 1024);
        assert_eq!(chunker.count(), 1);
    }

    /// The chunker is bounded even for empty blocks.
    #[test]
    fn the_chunker_is_capped() {
        let mut chunker = Chunker::new(usize::MAX / 2);
        chunker.observe(16, 16);
        assert_eq!(chunker.count(), Chunker::MAX);
    }

    /// A chunk of nothing does not move the estimate.
    #[test]
    fn an_empty_chunk_does_not_divide_by_zero() {
        let mut chunker = Chunker::new(1024);
        chunker.observe(0, 0);
        assert_eq!(chunker.count(), Chunker::FIRST);
    }
}
