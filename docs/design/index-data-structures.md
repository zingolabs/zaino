# Index data structures

Every RPC Zaino answers from its own indexes is a fold over one stream of blocks. This document
covers the shape of each fold: what it keeps on disk, and how a read finds it again. The substrate
is measured in [persistence-architecture.md](./persistence-architecture.md), the still-reorgable
tip in [nfs.md](./nfs.md), and which methods an index answers at
all in [boundaries.md](./boundaries.md).

## 1. There is one input

The only input is `getblock <hash> 0` (each hash named by the verified header chain), the raw
consensus bytes, which Zaino parses itself
(`zaino-source/src/decode.rs`). For each transaction that gives us:

```text
txid
transparent   coinbase flag, inputs [{ prev_txid, prev_index }], outputs [{ value, script }]
sprout        value balance
sapling       nullifiers [nf], outputs [{ cmu, epk, enc_ciphertext[..52] }], value balance
orchard       actions [{ nullifier, cmx, epk, enc_ciphertext[..52] }], value balance
ironwood      same shape as orchard
```

A transparent input carries only the outpoint it spends, never the address or the value. That one
gap is what §5 is about.

Everything we serve from an index is a fold over this stream, so the design question for each
index is only what the fold keeps, and what a read has to reconstruct from it. The mempool, the
validator's view of the network (`estimatedHeight`) and `SendTransaction` are not folds over
blocks, so they are not indexes.

## 2. Three shapes, of which we build two

|                             | Read addressed by                                 | Entries removed? | Structure                                    |
| --------------------------- | ------------------------------------------------- | ---------------- | -------------------------------------------- |
| **A: positional**           | a dense integer (height, position, subtree index) | no               | append-only file where the offset is the key |
| **B: associative, growing** | a key unrelated to append order                   | no               | immutable sorted segments, one per commit    |
| **C: associative, mutable** | a key unrelated to append order                   | **yes**          | a B-tree, or an LSM with tombstones          |

Shape A stores no keys at all, because arithmetic finds the record. Shape B stores its keys but only
needs a sparse index over them. Shape C needs deletes, and deletes bring compaction, space
amplification and an undo path for reorgs. The move that matters is turning every would-be Shape C
index into Shape B, which §5 does for the one case that looks mutable.

## 3. Shape A: positional

| Structure             | Key                     | Stride                                         | Files                          |
| --------------------- | ----------------------- | ---------------------------------------------- | ------------------------------ |
| compact block records | height                  | variable, through `offsets.idx` (8 B a height) | `blocks.dat`, `offsets.idx`    |
| commitment tree nodes | `(pool, level, slot)`   | 32 B                                           | `l00.dat` … `l31.dat` per pool |
| subtree roots         | `(pool, subtree index)` | 36 B: root, completing height                  | `subtrees.dat` per pool        |
| per-height tree sizes | height                  | 48 B: hash, time, one size per pool            | `heights.idx`                  |

The key is never stored. A record lives at `slot × stride`, a read is one slice of the mmap, and
the file is 100% dense. Compact blocks are the only variable-length records, so they are the only
structure with an offset table: `offsets.idx` holds where each height's record ends, and it starts
where the previous one ended. A height range is therefore one contiguous span of `blocks.dat`. The
records are the exact framed protobuf bytes that go on the wire, so serving a range moves bytes
without decoding or re-encoding them.

The commitment tree does not need keys either. A textbook incremental tree keeps only the latest
frontier, and a full tree keeps 2N nodes. We keep every leaf and every even-index internal node,
because an ommer is always the left sibling of a right child, and so always has an even index. That
is exactly the set of nodes any past frontier is built from, and it costs about 48 bytes per
commitment (32 for the leaf, about 16 for internal nodes). A historical `GetTreeState` is then at
most 33 mmap reads, the leaf plus one per set bit of its position, with no hashing. Rebuilding it
instead would cost 100.2 µs per Orchard `combine`
([persistence-architecture.md §3](./persistence-architecture.md#3-hcombine-measured)).

Subtree roots fall out of the same fold. Each time a pool completes a 2^16-leaf subtree, we append
its root and completing height at that subtree's slot.

## 4. Shape B: associative but append-only

Writes arrive in block order, but reads are by address or by outpoint. Each commit writes one
immutable segment per set: the rows of the blocks it covers, sorted by key, packed at a fixed
stride, then fences (the first key of every 4 KiB block of rows) and, for sets read by point
lookups, a membership filter (§7). Sorting costs nothing extra because the batch is in memory
anyway. `zaino_persistence::lsm` owns the format and the merge policy.

Each segment also stores a summary of its fences, the first key of every page of fences, which a
reader copies into memory when it maps the segment (about 1/100 of the fences). A read binary-searches
the summary in memory, reads the one page of fences it points to, then one 4 KiB block of rows,
and searches inside it: two page reads per segment however large the segment is. A range scan does
the same for its start key and walks forward from there.

A set's filter decides which segments a query visits at all. A point probe checks each segment's
filter first and touches no fence or row page on a miss. An address history filters on its address,
so it visits only the segments holding that address. Segments carry no height metadata, so a height
range only narrows where the seek lands inside each visited segment. What remains is the live
segment count, which bounds the filter checks per query, and that is why we merge.

Commits are frequent. An index commits when its batch reaches `batch_mib` (64 MiB by default)
during bulk sync, after every folded run at the tip (the NFS folded it already, usually one final
block), and whenever the final stream is quiet for a second (`IDLE` in zaino-sync's
`committer.rs`), so an unmerged set would gain a segment per mainnet block at the tip. Segments are therefore size-tiered: a segment's tier is `⌊log₈ rows⌋`, and once a tier holds
eight segments a background thread, one per tier, merges them into one segment of the tier above.
The index's next manifest commit swaps the merged segment in, and the inputs are unlinked only once
that manifest is durable. A commit never waits on a merge unless the merging tier falls two full
windows behind, at 24 segments (`STALL_WINDOWS` in `lsm/log.rs`), which bounds the fan-out a read
can see.

A merge here is not compaction. With no deletes and no updates it is a pure k-way merge of sorted
arrays, with no tombstones and no version conflicts, and correctness never depends on one. Segments
are also the state rather than a log of mutations to it, so an unmerged set costs read time but
never restart time.

A segment written once, sorted, is exactly as big as its rows. A B-tree must leave room for inserts
it cannot predict, and random-order inserts leave its pages part-full and, on a copy-on-write
filesystem, fragment the file
([persistence-architecture.md §4](./persistence-architecture.md#4-direct-addressing-beats-a-keyed-store-on-every-axis-measured)).

## 5. Shape C, and why it does not exist

One thing looks mutable. Recording a spend under the spending address, or knowing what a spent
output was worth, needs a map from outpoint to `(address, value)` that grows on every output and
shrinks on every spend.

The transparent-address index folds every block from genesis, so the prevout of any spend is an
output it has already written. That is also why it can never offer to start at a later height. It
does not need to resolve the prevout at write time at all. Instead it records each side under the
key the block already provides:

```text
receives   (addr, height, txid, vout) -> value        keyed by address,  Shape B
spent      (txid, vout) -> (height, spending_txid)    keyed by outpoint, Shape B
```

Both are pure projections of one block. The fold performs no lookups and keeps no outpoint map, no
carried state and nothing mutable. Queries compose the two sets instead. `GetAddressUtxos`
range-scans `receives` for the address, probes `spent` for each outpoint it found, and returns the
misses. `GetTaddressTransactions` takes the paying txids from `receives`, and probing `spent` for
those outpoints gives the spending txids and their heights.

|                   | Maintained UTXO set                          | Pure fold                    |
| ----------------- | -------------------------------------------- | ---------------------------- |
| Write path        | a lookup, an insert and a delete per input   | append only                  |
| Carried state     | ~1.4 GB resident, or a mutable on-disk index | none                         |
| Reorg             | must undo deletes                            | drop the non-finalized state |
| `GetAddressUtxos` | `O(unspent)`                                 | `O(received)`                |
| Storage engines   | two                                          | one                          |

We accept `O(received)` over `O(unspent)` because the gap only shows for a heavily reused address,
such as an exchange hot wallet, and not for a light wallet's mostly single-use receivers (§7). If it
ever matters, a maintained set can be added as an accelerator over the same segments without
changing the source of truth.

### Fees: the one lookup, still no Shape C

`CompactTx.fee` is the one value resolved at write time, because it is encoded into the compact
record, and its transparent term (the value of everything the inputs spend) exists nowhere in the
block. Every other term, the Sprout, Sapling, Orchard and Ironwood value balances, is in the
transaction. So some fold has to look up an outpoint's value once per transparent input.

`zaino-internal-value-balance` does that without becoming Shape C, by the same move: it never
deletes. Its `outputs` set maps `(txid, vout)` to a value and keeps spent outputs too, so it is a
probed Shape B set like `spent`. A reorg drops the non-finalized state with nothing to undo, and
any height resolves the same way every time, so an index downstream that is behind it replays
through the same lookups with no rewind. Every probe
hits, since a valid spend's prevout exists, so the filter picks the one segment holding it and a
probe costs one block read. At mainnet scale that is about 190M outputs × 44 B, or 8.4 GB, against
about 1.4 GB resident plus a delete path for a maintained UTXO set.

The value-balance index serves nothing itself. In bulk sync it publishes each block's
per-transaction fees into a `FeeSink`, and the compact-block index reads one fee step per unfolded
block, pairing each block with its fees by hash; at the tip the NFS folds value-balance first and
hands compact-block's fold the fees ([data-sink.md](./data-sink.md#indexes-publishing-to-other-indexes-fees)).

Mempool fees are not a fold. An unconfirmed transaction may spend another unconfirmed one, and the
validator already resolved both when it admitted them, so we take the fee from its
`getrawmempool true` listing ([boundaries.md](./boundaries.md)).

## 6. Completeness

| RPC                                                                      | Fold                              | Shape                        |
| ------------------------------------------------------------------------ | --------------------------------- | ---------------------------- |
| `GetLatestBlock`, `GetBlock`, `GetBlockRange`, `GetBlockRangeNullifiers` | compact record per height         | A                            |
| `CompactTx.fee` in those records                                         | outpoint to value (value-balance) | B                            |
| `GetBlock` and `GetTreeState` by hash                                    | block hash to height (`by_hash`)  | B                            |
| `GetTreeState`, `GetLatestTreeState`                                     | commitment frontier               | A                            |
| `GetSubtreeRoots`                                                        | same fold, at 2^16 boundaries     | A                            |
| `GetTaddressTransactions`                                                | address to (height, txid)         | B (bytes from the validator) |
| `GetAddressUtxos`, `GetAddressUtxosStream`                               | receives minus spends             | B                            |
| `GetTaddressBalance`, `GetTaddressBalanceStream`                         | the sum of the above              | B                            |

Every row is a fold over the block stream, so never forwarding a derived read is a property of the
design, not a policy we have to enforce. `GetTransaction` is not on the list: compact records are
lossy, and the full transaction bytes already live in the validator ([boundaries.md](./boundaries.md)).

Durable storage stops at `tip − finalised_depth`, but clients ask inside that window constantly:
librustzcash calls `GetTreeState` during steady-state polling, and pepper-sync asks address queries
over the last ~100 blocks. The window is the same fold, held in the NFS's per-block layers and not
yet committed ([nfs.md](./nfs.md)). Reorgs are absorbed in memory and never reach disk, which is
what lets Shapes A and B exist.

## 7. Shape B instances

| Set                       | Key                           | Row  | Read by                         |
| ------------------------- | ----------------------------- | ---- | ------------------------------- |
| `receives`                | `addr ‖ height ‖ txid ‖ vout` | 69 B | range scan, filtered by address |
| `spent`                   | `txid ‖ vout`                 | 72 B | point probe                     |
| `outputs` (value-balance) | `txid ‖ vout`                 | 44 B | point probe, always hits        |
| `by_hash` (block-hash)    | `block hash`                  | 36 B | point probe                     |

The address key is the address itself, not an id for it. `receives` is keyed by the 21-byte
`[hash160][kind]` tag. Interning it into a `u64` would cost a dictionary, a second structure to keep
in step and an indirection on every read, and only breaks even at about 2.9 rows per address. That
is more than a wallet receiver reaches, and far more than ZIP-320 TEX and librustzcash ephemeral
receivers, which are single-use by construction. Keys are big-endian, so byte order is key order and
segments compare encoded prefixes without decoding them.

The other three sets are probed by a uniformly distributed key and never scanned. An unspent output,
the common case, is a miss in every `spent` segment, so each probed segment carries a filter that
turns most of those misses into no read at all.

`receives` is range-scanned, and its filter covers only the 21-byte address at the front of each key
(`Key::FILTER_PREFIX`). An address history is a range whose start and end share that address, so
each segment's filter answers whether the address is there at all, and the scan skips every segment
that never saw it. Most addresses are active for a short stretch of the chain, so this turns a seek
in every segment into a seek in the few that hold the address. The address puts its hash first for
this reason: the filter shards on a key's first 8 bytes, which must be uniform.

Readers read and check each segment's filter when they map it, so a probe never faults in a cold
filter page, and the filters stay resident as the page cache allows.

Segments never change after they are built, which is what a static filter is for (RocksDB's
per-SST filters are the same idea), so we use a sharded BinaryFuse8. Mainnet has about 190M
transparent outputs (Blockchair, Sep 2026), so `spent` holds about 190M rows over up to ~35 live
segments (size tiers × fanout 8). Measured here with xorf 0.13, BinaryFuse8 and BinaryFuse16 both
build at about 100 ns per key and probe in about 17 ns, at 9 and 18 bits per key and false-positive
rates of 2⁻⁸ and 2⁻¹⁶. With 8-bit fingerprints a miss wastes at most 35 × 2⁻⁸, about 0.14 block
reads. The filters only stay cheap while resident, and at mainnet scale they cost about 200 MB
against 400 MB, so the smaller one wins. Build throughput does not separate them: it is about
400 ns per row over a row's life, across roughly log₈ merges, and sync is bound by validator RPC.

Each shard holds at most 2²⁰ keys, chosen by the key's top bits, which are uniform for a txid. That
bounds build scratch at about 26 MiB whatever the segment's size, and since shards close in key
order, a merge builds them as it streams.

| Shape | Primitive                                                              | Indexes                                                                          |
| ----- | ---------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| A     | append-only files and mmap, offset as key                              | `zaino-index-compact-block`, `zaino-index-tree-state`                            |
| B     | `zaino_persistence::lsm`: immutable sorted segments, fences and filter | `zaino-index-transparent-address`, block-hash `by_hash`, value-balance `outputs` |

There is no mutable on-disk structure in Zaino and no second storage engine. A new index is a
parameterisation of one of these two.
