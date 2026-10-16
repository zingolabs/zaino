# `backend-bench` — usage

Measures one persistence backend in isolation, over the `zaino-persistence`
seam. No source, no provisioner, no engine.

This is not the pipeline benchmark. `bench/sync-bench` measures a validator
through the provisioner and engine into a store, at cluster scale, and answers
"how fast does zaino sync". This crate answers a narrower question: **given the
same write and read instructions, what does this backend cost?** Use it when
changing a backend, a codec layout, or the batch shape.

## Running it

```bash
cargo bench -p backend-bench
```

Every workload runs against every backend in one pass, so the report reads as a
comparison:

```text
commit_batches/lmdb/typical-256b-64       time: [2.15 ms 2.37 ms 2.72 ms]
commit_batches/in-memory/typical-256b-64  time: [262 µs 266 µs 270 µs]
```

The name is `workload / backend / scale`. The scale is in the name on purpose:
criterion keys its stored results by name, so a run at a different scale cannot
be silently compared against one at another.

A shorter run, for a quick check rather than a number worth keeping:

```bash
cargo bench -p backend-bench -- --warm-up-time 1 --measurement-time 3 --sample-size 20
```

## Comparing across commits: criterion baselines

Save a baseline before the change, compare after it:

```bash
# on the commit you are comparing against
cargo bench -p backend-bench -- --save-baseline before
# … change the backend, the codec, or the batch shape …
cargo bench -p backend-bench -- --baseline before
```

The second run prints the change per benchmark with a p-value, so a regression
is named rather than eyeballed.

Two constraints on this, both of which invalidate a comparison if ignored:

- **`target/criterion` is per checkout.** Both runs must happen in the same
  worktree, or the baseline is simply not there. A worktree per branch means a
  baseline per branch.
- **Numbers are comparable only within one machine, in one session.** Nothing
  here is comparable across machines, across CPU governors, or against a number
  from a different day on a busy laptop. The only figure to quote elsewhere is a
  *ratio* between two backends measured in the same run.

## The workloads, and what each mirrors

| benchmark | what it mirrors in production |
|---|---|
| `commit_batches` | the sync engine's write path: every index's ops for a batch of blocks, plus the watermark, in one atomic commit, then one flush. Throughput is reported over the logical bytes written. |
| `read_by_height` | a served read that already knows the height — the common case, against a height-ordered namespace. |
| `read_by_hash` | the `hash → height` lookup a read by hash pays first, whose key is a 32-byte hash rather than a height, so it probes a different key distribution. |
| `compose_block` | the store's compose-on-read: the seven-namespace fan-out one served compact block is assembled from, at one height. |
| `compose_block_x{N}` | the same fan-out from N threads at once (`BACKEND_BENCH_THREADS`, default 16), a reader taken per block as the store does. The workload that shows contention between readers — a lock or a shared cache line — which every single-threaded body above is blind to. |
| `store_compose` | one served compact block through the real `zaino-store` `StoreReader`: the reader, each lookup's format-freshness check, the decodes and the assembly. Above the backend fan-out by the codec and store layers' cost. |
| `store_compose_x{N}` | `store_compose` from N threads, a snapshot per thread as each request has its own. |
| `store_range` | one `GetBlockRange` step: 100 consecutive compact blocks through the store's `stream_compact`, throughput per block. |
| `store_range_x{N}` | `store_range` from N threads, a snapshot per thread. |
| `scan_namespace` | the whole-namespace walk pipeline state loading does today — and the shape a ranged read would replace, so this is the number a `range` addition has to beat. |
| the `on-disk growth` lines | on-disk bytes beside logical bytes written, per backend, so write amplification is visible. Printed once at the start of a run, not timed. |

## What makes the numbers honest

- **Real bytes.** Records go through the production codecs, so key and value
  sizes are the indexer's. A codec layout change moves the benchmark with it.
- **Real batching.** One `commit` per batch with the watermark inside it, as the
  engine does — not one write per entry.
- **The flush is inside the measured section.** The engine commits each batch
  and flushes when the run completes, so the benchmark does too. A comparison
  that skipped it would credit a backend for work it had not finished.
- **A fresh store per write iteration.** `iter_batched` creates the backend and
  generates the records in setup, outside the timer, so a write measures growth
  rather than overwrite.
- **Deterministic input.** A seeded SplitMix64 generator, local to this crate
  rather than a `rand` dependency, so no version bump can change the stream two
  backends are compared on.
- **A correctness gate.** Every populated store is verified — each header entry
  read back and compared — before it is read from, so a backend cannot win by
  losing or mangling data.

## Scale knobs

Defaults are sized to finish in seconds: 256 blocks, batches of 64, `typical`
shape. Production commits 1000-block batches; a run that wants that shape sets
it. Every knob is read once at bench setup and appears in the benchmark name.

| variable | default | meaning |
|---|---|---|
| `BACKEND_BENCH_BLOCKS` | `256` | blocks generated and written |
| `BACKEND_BENCH_BATCH` | `64` | blocks per atomic commit |
| `BACKEND_BENCH_SEED` | `24301` | record generator seed |
| `BACKEND_BENCH_SHAPE` | `typical` | `light`, `typical` or `sandblast` |

`shape` is how much material a block carries, which is what makes a mainnet
block 1 KiB or 1 MiB:

- `light` — an early-chain block: a couple of transparent transactions, no
  shielded material. Per-key overhead dominates.
- `typical` — a mid-chain block: a handful of transactions across all pools.
- `sandblast` — a 2022-spam-era block: output-heavy shielded transactions. Value
  bytes dominate, and write amplification shows.

An unparseable value keeps the default rather than failing the run.

## Adding a backend

Implement `BenchTarget` for it — create fresh, hand out the `Backend`, report
on-disk bytes — and register it in each workload group in `benches/backend.rs`:

```rust
commit::<Positional>(&mut group, &scale);
```

Nothing else changes. The workloads are generic over the trait, so there is no
import to edit, no feature to flip, and no measured code that differs between
backends.

Two things a new target must state in its `BenchTarget` impl docs, because they
decide whether its numbers mean anything: what it reserves at open (LMDB
reserves an 8 GiB map up front, so that figure bounds what a run may write, not
what it does write), and what `flush` actually guarantees.
