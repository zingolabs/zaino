//! Index-writer test kit: a store on `SimFs`, steps, a spawned writer, random final streams
//! (`usage.md` "Testing a writer")

use std::{future::Future, num::NonZeroUsize, path::Path, sync::Arc};

use proptest::{
    collection, prop_oneof,
    strategy::{Just, Strategy},
};
use tokio::task::{JoinError, JoinHandle};
use zaino_persistence::{
    fs::SimFs, DiskEngine, DiskStore, LsmConfig, PersistenceEngine, Schema, View,
};
use zaino_primitives::types::{Block, Height};

use crate::{IndexHandle, IndexerDataSink, Step, Subscription};

/// Final-stream queue budget (one run = every step already queued)
pub const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("1 MiB is non-zero");

/// `schema`'s store at `/<index name>` on `fs`
///
/// - `write_buffer` = MIN: every applied block committed by the store itself
pub fn open(fs: &Arc<SimFs>, schema: &Schema, write_buffer: NonZeroUsize) -> DiskStore {
    let dir = Path::new("/").join(schema.kind.name());
    DiskEngine::new(fs.clone(), LsmConfig::default())
        .open(&dir, schema, write_buffer)
        .unwrap_or_else(|error| panic!("open {}: {error}", dir.display()))
}

pub fn step(block: &Arc<Block>) -> Step<Block> {
    Step::Apply { height: block.header().height, data: Arc::clone(block) }
}

/// `block` as the chain's final tip (the writer commits after it)
pub fn finalized(block: &Arc<Block>) -> Step<Block> {
    Step::Finalized { height: block.header().height, data: Arc::clone(block) }
}

/// Writer task on its own final stream, read through its handle
pub struct Writer<V> {
    sink: IndexerDataSink<Block>,
    handle: IndexHandle<V>,
    task: JoinHandle<()>,
}

impl<V: View> Writer<V> {
    /// `run` (the writer's loop over the subscription) spawned; `queue` = one run's bytes
    pub fn spawn<F>(
        handle: IndexHandle<V>,
        queue: NonZeroUsize,
        run: impl FnOnce(Subscription<Block>) -> F,
    ) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut sink = IndexerDataSink::new();
        let task = tokio::spawn(run(sink.subscribe("writer", queue)));
        Self { sink, handle, task }
    }

    pub async fn send(&self, step: Step<Block>) {
        self.sink.send(step).await;
    }

    pub fn handle(&self) -> &IndexHandle<V> {
        &self.handle
    }

    /// Until the durable tip = `tip` (`None` = nothing committed)
    pub async fn durable_at(&mut self, tip: impl Into<Option<Height>>) {
        let tip = tip.into();
        while self.handle.tip().map(|held| held.height) != tip {
            assert!(self.handle.changed().await, "writer alive until its durable tip = {tip:?}");
        }
    }

    /// `Shutdown`, then the task's end (`Err` = it panicked: `resume_unwind(error.into_panic())`)
    pub async fn stop(self) -> Result<IndexHandle<V>, JoinError> {
        self.sink.shutdown();
        self.task.await?;
        Ok(self.handle)
    }
}

/// One move of a random final stream over a fixed chain
///
/// - `Send(n)`: next `n` blocks, the last `Finalized`
/// - `Reopen`: writer restarted, the last block sent resent (held: skipped)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Send(usize),
    Reopen,
}

/// 1..16 moves, `Send(1..=3)` : `Reopen` = 4 : 1
pub fn moves() -> impl Strategy<Value = Vec<Move>> {
    let one = prop_oneof![4 => (1usize..=3).prop_map(Move::Send), 1 => Just(Move::Reopen)];
    collection::vec(one, 1..16)
}

/// `MIN` (every block its own commit) or `MAX` (commits at `Finalized` and `Shutdown` only)
pub fn write_buffers() -> impl Strategy<Value = NonZeroUsize> {
    prop_oneof![Just(NonZeroUsize::MIN), Just(NonZeroUsize::MAX)]
}

/// Random final stream's progress through `blocks` (genesis first)
pub struct FinalStream<'a> {
    blocks: &'a [Arc<Block>],
    sent: usize,
}

impl<'a> FinalStream<'a> {
    pub fn new(blocks: &'a [Arc<Block>]) -> Self {
        Self { blocks, sent: 0 }
    }

    /// Steps for `next` (a `Reopen`'s → the restarted writer; a `Send` past the end = what is left)
    pub fn steps(&mut self, next: Move) -> Vec<Step<Block>> {
        match next {
            Move::Send(count) => {
                let burst = &self.blocks[self.sent..(self.sent + count).min(self.blocks.len())];
                self.sent += burst.len();
                let last = burst.len().saturating_sub(1);
                let step_at = |(at, block)| if at == last { finalized(block) } else { step(block) };
                burst.iter().enumerate().map(step_at).collect()
            }
            Move::Reopen => {
                self.sent.checked_sub(1).map(|held| step(&self.blocks[held])).into_iter().collect()
            }
        }
    }

    /// Blocks sent: heights `0..sent`
    pub fn sent(&self) -> usize {
        self.sent
    }

    /// Last height sent (the durable tip once its move committed)
    pub fn tip(&self) -> Option<Height> {
        let last = self.sent.checked_sub(1)?;
        Some(self.blocks[last].header().height)
    }
}

/// `history` to its end on a current-thread runtime, clock paused (a proptest body is sync)
pub fn block_on_paused<F: Future>(history: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("current-thread runtime builds")
        .block_on(history)
}
