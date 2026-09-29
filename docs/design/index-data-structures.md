# Index data structures

How Zaino derives everything it serves from one input, so no derived read
forwards to the validator. This covers *shape*, what each fold retains;
[persistence-architecture.md](./persistence-architecture.md) measures the
substrate, [non-finalized-state.md](./non-finalized-state.md) covers the volatile tip,
[boundaries.md](./boundaries.md) decides which side of the boundary a method is
answered from.

## 1. There is one input

`getblock <height> 0`: raw consensus bytes, parsed by Zaino. Per transaction
that yields:

```
txid
transparent_inputs   [{ prev_txid, prev_index }]          <- outpoint only
transparent_outputs  [{ value, script }]
sapling_nullifiers   [nf]
sapling_outputs      [{ cmu, epk, enc_ciphertext_head }]
orchard_actions      [{ nullifier, cmx, epk, enc_ciphertext_head }]
ironwood_actions     [ same shape ]
```

Everything served is a **fold over that stream**. The design question for each
index is only: *what does the fold retain, and what does a read reconstruct?*

Not folds, and so not indexes: the mempool (not in blocks; a live polled view),
node liveness (`estimatedHeight`, the validator's own tip), and
`SendTransaction` (a write).

## 2. Three shapes, distinguished by what the read needs

|                             | Read addressed by                         | Entries removed? | Structure                                              |
| --------------------------- | ----------------------------------------- | ---------------- | ------------------------------------------------------ |
| **A: positional**           | a dense integer (height, position, index) | no               | append-only file, **offset = key**                     |
| **B: associative, growing** | a key unrelated to append order           | no               | immutable sorted segments, partitioned by append order |
| **C: associative, mutable** | a key unrelated to append order           | **yes**          | a real mutable index                                   |

Shape A stores **no** keys: arithmetic finds the record. Shape B carries a
sparse index only. Shape C pays for a B-tree or an LSM. **Converting C into B is
the move that matters**: it removes deletes, compaction, space amplification and
reorg-unwind complexity in one step.

## 3. Shape A: positional

| Structure             | Key                     | Stride                             | Files                          |
| --------------------- | ----------------------- | ---------------------------------- | ------------------------------ |
| compact block records | height                  | variable, via `offsets.idx` (16 B) | `blocks.dat`, `offsets.idx`    |
| commitment nodes      | `(pool, level, index)`  | 32 B                               | `l00.dat` … `l31.dat` per pool |
| subtree roots         | `(pool, subtree_index)` | 36 B                               | `subtrees.dat` per pool        |
| per-height tree sizes | height                  | 48 B                               | `heights.idx`                  |

The key is never stored: `offset = index × stride`, a read is one mmap slice, and
density is 100%. A fixed stride also removes the offset table and the read that
consults it; only compact blocks, being variable-length, need one.

**The commitment tree needs no keys either.** A frontier at any historical
position reconstructs from retained left-siblings: every ommer index is even,
and `append` retains exactly one node per call. So 32 bytes per node, addressed
by `(pool, level, index)`, and a historical read is ≤33 mmap slices with **no
hashing**, against a measured 100.2 µs per `combine`
([persistence-architecture.md §3](./persistence-architecture.md#3-hcombine-measured)).

**Subtree roots** are a byproduct of the same fold, emitted on 2^16 boundaries,
slot = subtree index.

## 4. Shape B: associative but append-only

Writes arrive in **block order**; reads are by **address**. Every address query
also carries a height range, so the write order is a query dimension too:

- **Partition by height**: free, that is the append order. Each batch flushes
  one immutable segment.
- **Sort by key within a segment**: the batch is in memory anyway.
- **Sparse in-memory index per segment**: every Nth key plus its offset.

A segment is a static, fixed-stride, perfectly packed sorted array. Segments are
height-disjoint and ordered, so a query selects them by binary search over segment
metadata. A hot query (pepper-sync asks over a ~100-block window) touches one
segment. A cold birthday→tip query touches every segment: 3,400 at `batch = 1000` over
3.4M blocks.

**Merging is not compaction.** With no deletes and no updates, a merge is a pure
k-way merge of sorted arrays: no tombstones, no version conflicts, no
write-stall. It is only ever a cold-read optimisation, so it can be deferred
indefinitely. `SegmentLog` runs merges on background threads, one per size tier, and
the next commit swaps each one in; no commit waits on a merge unless a tier falls
two windows behind (`zaino-persistence` usage guide).

**Density.** A B-tree must leave room for inserts it cannot predict; random-order
inserts leave pages part-full and, on a copy-on-write filesystem, fragment the
file ([persistence-architecture.md §4](./persistence-architecture.md#4-direct-addressing-beats-a-keyed-store-on-every-axis-measured)).
A segment written once, sorted, is exactly as big as its rows.

**Segments are the state, not a log.** A delta log describes *mutations to* state
and must be replayed on restart; a segment *is* state. So a never-merged segment set
grows only read cost, never restart cost.

## 5. Shape C, and why it does not exist

One thing looks mutable: recording a spend under the spending *address*, or
knowing what a spent output was worth, needs `outpoint → (address, value)`, a
map that grows on every output and shrinks on every spend.

This index sees every block from genesis, so **the prevout of any spend is an
output it already wrote** (which is why it must never offer a `start_height`).
And it need not resolve at write time at all. Record each side under the key the
block already gives:

```
receives   (addr, height, txid, vout) -> value        addr-keyed,     Shape B
spent      (txid, vout) -> (height, spending_txid)    outpoint-keyed, Shape B
```

Both are pure projections of one block. **The fold performs no lookups**: no
outpoint map, no carry, no mutable structure. Queries compose the two:

- `GetAddressUtxos(addr)`: range-scan `receives` for `addr`, probe `spent` for
  each outpoint, return the misses.
- `GetTaddressTransactions(addr, range)`: `receives` gives the paying txids;
  probing `spent` for those outpoints gives the spending txids and heights.

|                   | Maintained UTXO set                          | Pure fold                |
| ----------------- | -------------------------------------------- | ------------------------ |
| Write path        | lookup + insert + delete per input           | append only              |
| Carry             | ~1.4 GB resident, or a mutable on-disk index | none                     |
| Reorg unwind      | must undo deletes                            | drop non-finalized state |
| `GetAddressUtxos` | `O(unspent)`                                 | `O(received)`            |
| Storage engines   | two                                          | one                      |

`O(received)` against `O(unspent)` only matters for a heavily reused address (an
exchange hot wallet), not for a light wallet's own mostly single-use receivers
(§7). A maintained set could later be added as an *accelerator* over the same
segments without changing the source of truth.

### Fees: the one lookup, still no Shape C

`CompactTx.fee` is the exception to "resolve at read time": it is written into
the compact record when the block is encoded, and its transparent term (Σ of
what each input spends) exists nowhere in the block. Every other term (the
Sprout, Sapling, Orchard and Ironwood value balances) is in the transaction.
So a fold *must* look up `outpoint → value` once per transparent input.

`zaino-internal-value-balance` does that without becoming Shape C, by the same
move: **it never deletes**. `outputs (txid, vout) → value` keeps spent outputs
too, so it is a probed Shape B set like `spent`:

- append-only: reorg = drop the non-finalized state, no undo; a commit is one segment
- any height re-resolves identically, so an index downstream that is behind
  it replays through the same lookups with no rewind
- every probe hits (a valid spend's prevout exists): the filter picks the one
  segment holding it, so it costs one block read, not a miss per segment
- mainnet ≈ 190M outputs × 44 B ≈ 8.4 GB, against ~1.4 GB resident plus a
  delete path for a maintained UTXO set

Its output is not served. Each block's per-transaction `Fee` goes into a
`FeeSink`, which the compact-block index pairs with the block by height and
hash (`docs/design/sync.md`).

Mempool fees are not a fold: an unconfirmed transaction may spend another
unconfirmed one, and the validator already resolved both admitting them, so the
fee comes from its `getrawmempool true` listing ([boundaries.md](./boundaries.md)).

## 6. Completeness

| RPC                                           | Fold                             | Shape                                 |
| --------------------------------------------- | -------------------------------- | ------------------------------------- |
| `GetLatestBlock`, `GetBlock`, `GetBlockRange` | compact record per height        | A                                     |
| `CompactTx.fee` in those records              | outpoint → value (value-balance) | B                                     |
| `GetTreeState`, `GetLatestTreeState`          | commitment frontier              | A                                     |
| `GetSubtreeRoots`                             | same fold, 2^16 boundaries       | A                                     |
| `GetTaddressTransactions`                     | address → (height, txid)         | B (bytes hydrated from the validator) |
| `GetAddressUtxos`(+`Stream`)                  | receives − spends                | B                                     |
| `GetTaddressBalance`(+`Stream`)               | sum of the above                 | B                                     |

Every row is a fold over the block stream, so "no passthrough" is a design
property rather than a policy. `GetTransaction` is not on the list: compact
records are lossy, and full transaction bytes already live in the validator
([boundaries.md](./boundaries.md)).

Durable storage stops at `tip − finalised_depth`, but clients ask inside that
window constantly (librustzcash's `GetTreeState` during steady-state polling,
pepper-sync's address queries over the last ~100 blocks). The window is the same
fold, applied and not yet committed ([non-finalized-state.md](./non-finalized-state.md)):
reorgs are absorbed in memory and never reach disk, which is what lets Shapes A
and B exist.

## 7. Shape B instances

| Structure                 | Key                           | Range scans? | Point lookups?         |
| ------------------------- | ----------------------------- | ------------ | ---------------------- |
| `receives`                | `addr ‖ height ‖ txid ‖ vout` | **yes**      | no                     |
| `spent`                   | `txid ‖ vout`                 | no           | **yes**                |
| `outputs` (value-balance) | `txid ‖ vout`                 | no           | **yes** (always a hit) |

**The address key is the address, not an id for it.** `receives` is keyed by the
21-byte `[kind][hash160]` tag. Interning it into a `u64` would cost a
dictionary, a second structure to keep in step and an indirection per read, and
only breaks even at ~2.9 rows per address: above what a wallet receiver
reaches, and far above ZIP-320 TEX and librustzcash ephemeral receivers, which
are single-use by construction. Keys are big-endian, so byte order is key order
and segments compare encoded prefixes without decoding.

`receives` is range-scanned, so it stays sorted. `spent` is probed by a
uniformly distributed key and never scanned, and an *unspent* output (the common
case) is a miss in every segment, so each `spent` segment carries a filter.

**Filter: BinaryFuse8, sharded.** Segments are built once and never change, which
is what a static filter is for (RocksDB's per-SST filters are the same idea).
Sized from first principles:

- mainnet ≈ 190M transparent outputs (Blockchair, Sep 2026), so `spent` ≈ 190M
  rows over up to ~35 live segments (size tiers × fanout 8)
- measured here (xorf 0.13): build ≈ 100 ns/key and probe ≈ 17 ns for both
  BinaryFuse8 and BinaryFuse16; 9.0 vs 18.0 bits/key; FPR 2⁻⁸ vs 2⁻¹⁶
- a miss costs ≤ 35 × 2⁻⁸ ≈ 0.14 wasted block reads with 8-bit fingerprints,
  ~0 with 16; the filters cost ≈ 200 MB vs ≈ 400 MB at mainnet scale, and must
  stay resident to be cheap, so the smaller one wins
- build throughput does not separate them (≈ 400 ns per row over its life,
  ~log₈ merges, against a sync bound by validator RPC)
- shard = the key's top bits (a txid prefix is uniform), ≤ 2²⁰ keys each: build
  scratch bounded at ~26 MiB whatever the segment's size, and shards close in key
  order, so a merge builds them while it streams

The same probed-segment shape keys the block-hash index's `by_hash` locator
(block hash → height), which `GetBlock` and `GetTreeState` by hash resolve
through.

| Shape | Primitive                                                            | Indexes                                                                          |
| ----- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| A     | append-only files + mmap, offset-as-key                              | `zaino-index-compact-block`, `zaino-index-tree-state`                            |
| B     | `zaino_persistence::lsm`: immutable sorted segments, fences + filter | `zaino-index-transparent-address`, block-hash `by_hash`, value-balance `outputs` |

There is no mutable on-disk structure in Zaino and no second storage engine. A
new index is a parameterisation of one of these two.
