//! The store's compose-on-read, over a populated backend.
//!
//! [`compose_block`](crate::workloads::compose_block) measures the backend's
//! share of serving a compact block: seven raw lookups. This measures the whole
//! store read the server makes, through the real [`StoreReader`]: the reader it
//! takes per block, the format-freshness check each lookup makes, the decodes
//! and the assembly into a domain [`CompactBlock`]. The difference between the
//! two is the codec and store layers' cost, which a backend-only number cannot
//! show.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zaino_indexes::sets::compact_blocks::CompactBlocks;
use zaino_persistence::Backend;
use zaino_primitives::types::{BlockSelector, CompactBlock, Height};
use zaino_service::{CompactBlockRead, TakeSnapshot};
use zaino_store::{StoreReader, StoreSnapshot};

use crate::error::BenchError;
use crate::target::BenchTarget;
use crate::workloads::Populated;

/// A store snapshot over `store`'s backend, as the server takes one per request.
pub fn snapshot<T: BenchTarget>(
    store: &Populated<T>,
) -> Result<StoreSnapshot<T::Backend, CompactBlocks>, BenchError>
where
    T::Backend: Clone + 'static,
{
    let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(store.backend().clone()));
    futures::executor::block_on(reader.snapshot())
        .map_err(|e| BenchError::Generate(format!("store snapshot: {}", e.0)))
}

/// The generated height of block `index`, wrapping as the key accessors do.
pub fn height<T: BenchTarget>(store: &Populated<T>, index: usize) -> Height {
    let blocks = store.blocks().max(1);
    let offset = u32::try_from(index % blocks).unwrap_or_default();
    Height::try_from(offset).unwrap_or(Height::GENESIS)
}

/// One served compact block at `height`, through the store.
pub fn store_compose<B>(
    snapshot: &StoreSnapshot<B, CompactBlocks>,
    height: Height,
) -> Result<CompactBlock, BenchError>
where
    B: Backend + 'static,
{
    futures::executor::block_on(snapshot.compact_block(BlockSelector::Height(height)))
        .map_err(|e| BenchError::Generate(format!("store read: {e}")))?
        .ok_or_else(|| BenchError::Generate(format!("no block at {}", u32::from(height))))
}

/// [`store_compose`] from `threads` threads at once, each over its own
/// snapshot, composing `per_thread` blocks. Returns the wall time of the run.
pub fn store_compose_concurrent<T>(
    store: &Populated<T>,
    threads: usize,
    per_thread: usize,
) -> Result<Duration, BenchError>
where
    T: BenchTarget + Sync,
    T::Backend: Clone + 'static,
{
    let snapshots = (0..threads)
        .map(|_| snapshot(store))
        .collect::<Result<Vec<_>, _>>()?;
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<(), BenchError> {
        let workers: Vec<_> = snapshots
            .iter()
            .enumerate()
            .map(|(thread, snapshot)| {
                scope.spawn(move || -> Result<(), BenchError> {
                    for step in 0..per_thread {
                        let index = thread.wrapping_mul(7919).wrapping_add(step);
                        std::hint::black_box(store_compose(snapshot, height(store, index))?);
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| BenchError::Generate("a store thread panicked".to_owned()))??;
        }
        Ok(())
    })?;
    Ok(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::{height, snapshot, store_compose};
    use crate::data::Shape;
    use crate::target::{InMemory, Lmdb};
    use crate::workloads::Populated;

    /// The generated data reads back through the real store: every block is
    /// present, stamped fresh, and its per-pool records align.
    #[test]
    fn generated_blocks_compose_through_the_store() {
        fn check<T: crate::target::BenchTarget>()
        where
            T::Backend: Clone + 'static,
        {
            let store = Populated::<T>::build(Shape::Typical, 7, 40, 16).expect("populate");
            let snapshot = snapshot(&store).expect("snapshot");
            for index in 0..store.blocks() {
                let block = store_compose(&snapshot, height(&store, index)).expect("composes");
                assert_eq!(block.height, u32::try_from(index).expect("small"));
            }
        }
        check::<Lmdb>();
        check::<InMemory>();
    }
}
