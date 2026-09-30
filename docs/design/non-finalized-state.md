# The non-finalized state

We think of sync as a function, `f(old_state, blocks)`. The non-finalized state is simply that same
function applied to blocks that are not final yet, and kept in memory instead of committed. It is
not a second data structure or a separate subsystem. Every index keeps it in its own loop
([usage.md](../../packages/zaino-sync/usage.md#an-index-loop)): a non-final block is applied, and
dropped again on a reorg. Each index's own `usage.md` describes how it does so.

## Two watermarks

Every index tracks two heights. `applied_height` is how far its non-finalized state reaches, which
is the chain tip. `finalized_height` is how far it is durable on disk, which trails the tip by
`finalised_depth`.

`apply` extends the non-finalized state by one block. `finalize` and `committed` write final blocks
to disk, and only blocks below `tip − finalised_depth`, so nothing that can still be reorged is ever
fsynced. During bulk sync every block arrives already final and never goes through `apply`, so a
node catching up folds each block exactly once. All movement between the two tiers happens in
`committed`, which means a reader always finds a block in exactly one of them.

`reset` drops the entire non-finalized state. It only touches memory: durable structures never
delete anything, which is what keeps both of our storage shapes
([index-data-structures.md](./index-data-structures.md)) append-only.

`view` publishes an immutable snapshot of the non-finalized state. Readers pin it through `ArcSwap`
and check it before going to disk.

A reorg is a `reset` followed by applying the winning branch. A restart starts with an empty
non-finalized state and applies everything above `finalized_height`. Those are the same operation,
so the rarely exercised reorg path actually runs on every boot.

## Why `reset` takes no fork height

We could rewind to the fork point instead, but it is more code for less certainty. Every index would
have to agree on whether the fork height itself is kept or dropped, on the path that is hardest to
test. Every index would also need a reverse fold, the one direction its data structure is not built
for and that no read path exercises. Tree state would additionally have to re-read durable nodes to
rebuild its frontier.

Discarding everything instead means state only ever moves forward from a durable point, so there is
no reverse fold to get wrong. The cost is re-folding up to `finalised_depth` blocks on an event that
is rare by design, which we are happy to pay.

## Per index

| Index | Non-finalized state (`imbl`) | Carry |
|---|---|---|
| compact block | `OrdMap<height, Bytes>` of encoded records, plus hashes | cumulative tree sizes |
| tree state | retained nodes by `(level, slot)`, per-height sizes, subtree roots | frontier |
| transparent address | `OrdMap<ReceiveKey, _>`, `OrdMap<SpentKey, _>` | none |

An index with a carry keeps two copies of it, one after the last applied block and one after the
last finalized block, so `reset` is a simple assignment rather than a read from disk. Reads merge
the non-finalized state with durable storage through one pinned view per request; the compact-block
`ReadView::block` is the pattern to follow. The window holds `finalised_depth` blocks, 1,000 by
default, with one persistent-structure clone per published block.

## A reorg deeper than `finalised_depth` is fatal

The chain head refuses any fork below its window (`AdvanceError::BelowWindow`), and zainod exits. On
the next boot the stored tip hash no longer links to the validator's chain, so the first delivered
block fails the producer's link check (`ProduceError::Unlinked`), and zainod refuses to run until the
index is resynced. With `finalised_depth` at least `MAX_BLOCK_REORG_HEIGHT`, such a reorg is outside
the consensus rules, so we want a loud resync rather than silently splicing a new branch onto the old
durable prefix ([durability.md](./durability.md) §5).
