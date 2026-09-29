# The non-finalized state

Sync is `f(old_state, blocks)`. The non-finalized state is that same `f`,
applied and not yet committed: not a second structure, a second fold, or a
second subsystem. The trait lives in `zaino-sync`; each index's `usage.md` says
how that index realises it.

## 1. One fold, two watermarks

```
applied_height     non-finalized state extends to here  (tip)
finalized_height   durable to here                      (tip - finalised_depth)
```

- **`apply(block)`** extends the non-finalized state.
- **`finalize(blocks)`** prepares `blocks` for durable storage and returns the
  `write` that appends them; **`committed(done)`** lands it. They sit below
  `tip - finalised_depth`, so nothing reorg-able is ever fsynced. A block here
  need not have been applied: during bulk sync everything arrives already final
  and skips the non-finalized state, so a catching-up node folds each block
  **once**. The write runs off the follower (one at a time) while delivery and
  `apply` carry on; every tier change lands in `committed`, so readers see a
  block in exactly one tier.
- **`reset()`** drops *all* non-finalized state, returning to the durable tip.
  Memory only: **durable structures never delete**, which is what keeps
  both storage shapes ([index-data-structures.md](./index-data-structures.md))
  append-only.
- **`view()`** publishes an immutable snapshot of the non-finalized state;
  readers pin it through `ArcSwap` and consult it before the durable structures.

A reorg is `reset()` then re-apply the winning chain. A restart is "the
non-finalized state is empty, re-apply from `finalized_height`". **They are the same operation**, so
the rare reorg path is the path that runs on every boot.

### Why `reset()` carries no height

Unwinding to a fork point (`rewind_to(fork)`) is strictly more code for strictly
less certainty:

- Every index must agree whether `fork` is kept or dropped, on the path hardest
  to test.
- Every index needs a reverse fold: the one direction its data structure is not
  built for, and the one no read path exercises.
- Tree state would have to re-read durable nodes to reseed its frontier.

Discarding everything makes state *only ever move forward from a durable point*.
No reverse fold exists, so none can be wrong. The cost is re-fetching and
re-folding up to `finalised_depth` blocks on an event that is rare by design.

## 2. Per index

| Index               | Non-finalized state (`imbl`)                                       | Carry                 |
| ------------------- | ------------------------------------------------------------------ | --------------------- |
| compact block       | `OrdMap<height, Bytes>` of encoded records, plus hashes            | cumulative tree sizes |
| tree state          | retained nodes by `(level, slot)`, per-height sizes, subtree roots | frontier              |
| transparent address | `OrdMap<ReceiveKey, _>`, `OrdMap<SpentKey, _>`                     | none                  |

An index with a carry keeps it twice, after the last *applied* block and after the
last *finalised* one, so `reset` is an assignment rather than a read off disk.

Reads merge the non-finalized state then durable, in one pinned view per request
(the compact-block `ReadView::block` is the pattern).

The non-finalized state spans `finalised_depth` blocks (`MAX_BLOCK_REORG_HEIGHT` = 1000 by
default), one persistent-structure clone per published block.

## 3. The trait

```rust
pub trait IndexWriter: Send + 'static {
    type Input: Linked + Weight + Send + Sync + 'static; // item of the sink it subscribes to
    type View: Clone + Send + Sync + 'static;   // pinned by readers, O(1) clone
    type Error: std::error::Error + Send + Sync + 'static;
    type Done: Send + 'static;                  // a finished write, back to `committed`
    const NAME: &'static str;

    // tips = last height, inclusive; None = nothing held
    fn finalized_tip(&self) -> Option<BlockRef>;     // last durable block, the resume point
    fn finalized_height(&self) -> Option<Height>;    // provided: finalized_tip()'s height
    fn applied_height(&self) -> Option<Height>;      // last applied; `apply` expects the next
    fn view(&self) -> Self::View;

    async fn deliver(&mut self, blocks: &[Arc<Self::Input>]) -> Result<(), Self::Error>;
    async fn apply(&mut self, block: &Arc<Self::Input>) -> Result<(), Self::Error>;
    async fn finalize(&mut self, blocks: &[Arc<Self::Input>])
        -> Result<impl FnOnce() -> Result<Self::Done, Self::Error> + Send + 'static, Self::Error>;
    async fn committed(&mut self, done: Self::Done) -> Result<(), Self::Error>;
    async fn reset(&mut self) -> Result<(), Self::Error>;
}
```

Each subscriber's queue carries a step rather than a block, so a reorg travels
the same path as a block and cannot race it:

```rust
pub enum Step<T> {
    Apply { height: u64, data: Arc<T> },
    Reset,        // no height: the index returns to its own durable tip
}
```

## 4. Where the steps come from

- **`Apply`**: the `Producer` adds every block to the `BlockSink`
  (`IndexerDataSink<Block>`), which hands it to each subscriber waiting for
  exactly that height, bulk and tip alike
- **`Reset`**: the chain head resolves a reorg → `BlockSink::reset`, each
  subscriber's cursor rewinds to its first non-final height, and the producer
  replays the winning branch from the chain head's in-memory window

One `BlockSink`, one ordered subscription per index (`IndexFollower`), one fold.
The chain head decides the best chain (following chainview's quorum tip); it
derives no index state. See [sync.md](./sync.md).

## 5. A reorg deeper than `finalised_depth` is fatal

`reset` replays from the first non-final height, and durable files never
delete. The chain head holds the non-final window, so a fork below it is
refused (`AdvanceError::BelowWindow`): the producer ends and zainod exits. On the
next boot each index's stored tip hash no longer matches the validator's chain,
so the first delivered block fails the follower's linkage check
(`FollowError::Unlinked`) and zainod refuses to run until the index is resynced.
On mainnet and testnet the depth is at least `MAX_BLOCK_REORG_HEIGHT` (1000), so
a deeper reorg is outside consensus assumptions: a loud resync, never a silent
splice onto the old durable prefix. See [durability.md](./durability.md) §5.
