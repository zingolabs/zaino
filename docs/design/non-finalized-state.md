# The non-finalized state

We think of sync as a function, `f(old_state, blocks)`. The non-finalized state is simply that same
function applied to blocks that are not final yet, and kept in memory instead of committed. It is
not a second data structure or a separate subsystem. Every index's own loop
([usage.md](../../packages/zaino-sync/usage.md#an-index-loop)) holds it in
`zaino_persistence::Tiered`: a non-final block is applied, and dropped again on a reorg.

## Two watermarks

Every index tracks two heights. `applied_height` is how far its non-finalized state reaches, which
is the chain tip. `finalized_height` is how far it is durable on disk, which trails the tip by
`finalised_depth`.

`apply` extends the non-finalized state by one block. `finalize` and `committed` write final blocks
to disk, and only blocks below `tip − finalised_depth`, so nothing that can still be reorged is ever
fsynced. During bulk sync every block arrives already final and is staged instead (batched into
one commit), so a node catching up folds each block exactly once. All movement between the two
tiers happens in `committed`, which means a reader always finds a block in exactly one of them.

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

The non-finalized state is not per-index code: every index holds it in `zaino_persistence::Tiered`
([persistence-engine.md §5](./persistence-engine.md#5-tiering)). A block's effect on an index is
one `Changes`, the same one a commit writes, so the held tier is keyed exactly like storage and a
read resolves a position or key held first, then durable, through one pinned `LayeredView` per
request. `apply` and `stage` are the two watermarks' inputs, `finalize` is `committed`, `reorg` is
`reset`. The window holds `finalised_depth` blocks, 1,000 by default, with one persistent-structure
(`imbl`) clone per published block.

| Index               | One block's `Changes`                                         | Parent state read     |
| ------------------- | ------------------------------------------------------------- | --------------------- |
| compact block       | its encoded record                                            | cumulative tree sizes |
| tree state          | its height record, the nodes and subtree roots it completes   | frontier per pool     |
| transparent address | its `receives` and `spent` rows                               | none                  |
| value balance       | its transparent outputs                                       | none                  |
| block hash          | its `hash → height` row                                       | none                  |

An index's fold reads that state through its reader on the parent, off the view's tip record
(compact-block's `chainMetadata`, tree-state's frontiers through the same reconstruction a read
uses). Nothing is carried between blocks, so `reset` needs no step of its own: a reorg runs the boot
path.

## A reorg deeper than `finalised_depth` is fatal

The header chain never moves its final tip back, so the producer never sees such a fork: a final
block never changes (asserted). If the header store is lost and rebuilt onto another chain, the
first final tip covering an index's stored tip names a different block there, the producer stops
with `ProduceError::Diverged` before sending anything, and zainod refuses to run until the index
is resynced. With `finalised_depth` at least `MAX_BLOCK_REORG_HEIGHT`, such a reorg is outside
the consensus rules, so we want a loud resync rather than silently splicing a new branch onto the old
durable prefix ([durability.md](./durability.md) §5).
