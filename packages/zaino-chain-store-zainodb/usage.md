# `zaino-chain-store-zainodb` — usage

ZainoDB: the LMDB-backed implementation of the
[`zaino-chain-store`](../zaino-chain-store/usage.md) ports, and the on-disk
vocabulary it is built from.

**Depend on the ports, not on this crate.** The whole point of the split is that
a different store can be substituted by satisfying the same traits. Name this
crate only where you are constructing one.

```rust
use zaino_chain_store_zainodb::store::FinalisedState;

let store = FinalisedState::spawn(config, source).await?;
store.build_to(target).await?;
// Everything after this should be reached through a `ChainStoreReader`.
```

## Where things are

The port implementations live in `adapter`, over four modules: `error_map` for
this backend's failures as the domain names them, `to_domain` for reading,
`from_domain` for writing, and `history` for the feature-gated transparent
history. The two conversion directions are named for the direction and not for
the types they produce — `StoredBlock` is a *domain* type, so "into stored"
would mean conversions *out of* this crate, which is the opposite of what it
sounds like.

The module is called `adapter` rather than `ports` deliberately: the ports are
defined in `zaino-chain-store`, and this crate only satisfies them. Read the
contracts there.

## Spawning: two configs, and no path in this one

`FinalisedState::spawn` takes `ChainStoreConfig` and `ZainoDbConfig`.
`ZainoDbConfig` holds only what the neutral half cannot: the LMDB sizing and
write-cadence budgets, and the network whose activation schedule decides which
commitment-tree roots a block should have.

It deliberately carries **no path**. Where the store lives is
`ChainStoreConfig::path`, and passing no path is what selects the ephemeral
passthrough — so the two cannot contradict each other, because there is only one
field. `ZainoDbConfig::from_storage` reads the budgets out of an operator's
`StorageConfig` and ignores its `path` for exactly that reason.

## The on-disk types are a compatibility contract

Everything in `types` is a shape that has already been written to somebody's
disk. Its layout is not an implementation detail you can tidy.

Change one only by adding a body-format version and **leaving the old decoder in
place**. Each type sits with its encoding and the golden test that pins its
bytes, in the same file, so that a change to a shape and a change to what it
serialises to cannot land in separate commits.

If you find yourself editing a `types` file and no golden vector fails, that is
the warning sign, not the green light.

These are also *this backend's* shapes, not the domain's. What is currently
re-exported for `zaino-state` is a migration measure with an end date. Do not
add consumers.

## The checksums are load-bearing

The environment is opened `MDB_NOSYNC`. The documented consequence is that on
networked or overlay storage, or a hard pod eviction, a crash **can leave torn
pages**. Per-row BLAKE2b-256 over `encoded_key ‖ encoded_value` is what turns
that into `"checksum mismatch"` plus a hex dump and a "wipe and re-index"
instruction, rather than a wrong answer served with confidence.

Two properties to preserve when touching any of it:

- **The key binding.** The checksum covers the key as well as the value, which
  is what defeats relocating or splicing a row that is individually valid.
- **The version-searching `verify`.** It is what makes mixed-version rows in one
  table safe, which is the exact bug the v1.0.0→v1.1.0 migration exists to
  record as fixed.

## A schema mismatch rebuilds the database

There are no migrations. The `metadata` record holds the schema version and
schema hash of the build that created the database. When `spawn` finds a record
that differs from its own, or one it cannot decode, it deletes the database
directory and resyncs from the validator. Upgrades and downgrades take the same
path, and the index is derived data, so the rebuild loses nothing.

Any change to an on-disk encoding therefore bumps `DB_VERSION_V1`, and every
deployment pays one full rebuild on its next start. A failing golden in
`golden.rs` is the signal that a change carries that cost.

## The ephemeral backend has two jobs, not one

It is the passthrough for a deployment configured with no database. It is *also*
the read shim `init_or_take_ephemeral` installs while a long build or migration
is in progress. Removing the second role means a store 100k blocks behind
returns `None` for every read, which is worse than being slow.

Both roles report `Provenance::Passthrough`, and that is load-bearing rather
than cosmetic: the watermark bound is skipped for a passthrough read, because
the answer comes from the validator and the store's own durable tip is not its
limit. `watermark_provenance` derives from `finalised_state_mode`, which derives
from what `Router::backend` routes on — so it cannot disagree with where a read
actually lands. Anything that decides provenance from the primary backend alone
gets the *routed*-ephemeral case wrong, and a persistent store part-way through
a build then describes its passthrough answers as durable and refuses them.

## The watermark is published by whatever moves the tip

Every operation that can move the tip publishes: `write_block`, `rewind_to`, the
delete paths, spawn, and — the one that was missing — the completion of a build
run. The reads bounded by the watermark are unusable without it: a store that
built a hundred thousand blocks and never published would report no tip and
refuse every bounded read, while the database filled up behind it.

If you add a path that writes blocks, publish from it. `refresh_watermark` is a
free function taking the router precisely so the static build path can reach
it.

## Writing: append-only, contiguous, and batched where it can be

The writer requires `db_tip_height + 1`. It is strictly append-only; `rewind_to`
is a repair path, not part of following the chain.

Chainwork is typed, not checked:

- `BlockContext`, `BlockHeaderData` and `IndexedBlock` take a `Work` parameter:
  `AbsoluteChainWork` (known) or `Option<AbsoluteChainWork>` (default).
- Only the `AbsoluteChainWork` form has a stored encoding.
- `DbWrite::write_block` takes `IndexedBlock<AbsoluteChainWork>`.
- `conversion::chainwork_from_parent` errors on an unknown parent above
  genesis; the finalised build path stops there.
- `StoredBlockRead` answers from the v1 backend only and yields the known
  form; `IndexedBlockExt` answers from either backend and yields the `Option`
  form.
- `map_chainwork(Some)` widens a known block for a path that serves both.

`ChainStoreFreezeSink::freeze` takes a slice, and the adapter dispatches:
`write_block_batch_blocking` when it can, and the per-block path when
`transparent_address_history_experimental` is on, because that feature's
prev-output resolution cannot see earlier-in-batch uncommitted blocks. The
batch form sorts index entries before insert, so random-keyed `spent` and
`txid_location` writes become a sequential B-tree sweep instead of random page
faults once the database exceeds RAM.

The freeze stream feeding it is **best-effort**: it has gaps (subscriber lag,
restart, the zero-receiver window) and duplicates (a reorg that lowers the tip
and re-advances). Ingest must be idempotent on `(height, hash)`, and the
source-driven build stays the authority — freeze only spares it the fetch.

## What the read path reports

Emission unconditional; no recorder (no `zainod` `prometheus` feature) → no-op.

| Metric                        | Kind      | Watch it for                                         |
| ----------------------------- | --------- | ---------------------------------------------------- |
| `zaino.db.read_seconds{op}`   | histogram | per-op read latency; `op="compact_chunk"` = wallet sync |
| `zaino.db.corrupt_rows_total` | counter   | non-zero = database damaged, not behind              |

- Timed per *chunk* (one read txn spans the range; per-block = one duration ÷ a count)
- Recorded on success and failure (a slowly failing store shows as slow)
- Alert on `corrupt_rows_total`: an undecodable row falls through to the validator → correct
  answer, silent rot
- Paired `warn!` names the rejecting conversion, Prometheus or not:

```
WARN chain store read a row it cannot decode
     error=chain store holds a corrupt row: expected in-range value for stored output 21000000000000001
```

- Both from one helper in `adapter::error_map` (a new conversion reports unasked)

## Testing against the vector chain

The `testing` feature (dev-dependency only; `resolver = "2"` keeps it out of
production graphs) exposes `tests::vectors` and `tests::fixtures` so consumers
run against the same chain this crate's own suites do — one oracle, not two
copies of one.

For a test that needs a database *at* a height, use
`fixtures::fill_store_with_blockdata` rather than `build_to`. It writes the
vector chain block-by-block; `build_to` runs the store's ingest, which wakes the
background validator hard enough to dominate a seed build under a parallel test
runner. Those fixtures bypass the store's ingest and so must republish the
watermark themselves, which they do — a fixture that leaves the store in a state
no real build produces makes every watermark-bounded read in the test refuse.

### The port suite is differential, and should stay that way

`tests::finalised_state::ports` asks each question twice — once through a
`zaino-chain-store` port, once through the inherent read it replaces — and
requires the answers to agree, plus a freeze round trip that reads a chain out
of one store and writes it into an empty one. A conversion layer has no
self-evident correct answer, but it has a known-good one, and comparing against
it is the only check that does not simply restate the conversion in the
assertion.

The freeze round trip is the half that catches the expensive class. A field the
read drops is invisible to a read-only test — the value simply never appears —
and shows up only when something writes the result back down. It has already
caught non-standard address keys round-tripping to zeroes and per-pool value
balances round-tripping to `None`.

The comparison stops being available when the inherent reads are deleted. That
is the right moment to lose it, and not before.

## Implementing the stream ports

`blocks_stream` and `compact_stream` return an opaque `impl Stream`, not a boxed
one, so nothing allocates to hand a stream across the port. Two consequences
land on the implementation rather than the caller.

**One concrete stream type per method.** You cannot return `stream::empty()`
from one arm and a cursor walk from another — they are different types and the
return is a single opaque one. Fold the branch into the walk's own state
instead:

```rust
// `range` is `None` when the whole request sits above the watermark. The empty
// case is a walk with nothing to walk, so it belongs inside the walk.
fn chunked<B, F, Fut>(range: Option<(Height, Height)>, read: F)
    -> impl Stream<Item = Result<Vec<B>, ChainStoreError>> + Send
{
    let (cursor, end) = match range {
        Some((start, end)) => (Some(start), end),
        None => (None, Height(0)),
    };

    futures::stream::try_unfold((cursor, read), move |(cursor, mut read)| async move {
        let Some(from) = cursor else { return Ok(None) };
        let to = Height(from.0.saturating_add(CHUNK - 1).min(end.0));
        let chunk = read(from, to).await?;
        let next = (to.0 < end.0).then(|| Height(to.0 + 1));
        Ok(Some((chunk, (next, read))))
    })
}
```

**The stream may not borrow `&self`.** The port declares `use<Self>`, which
excludes the `&self` lifetime, and an implementation must echo it (`use<T>` on
the impl — omitting it is a distinct *"return type captures more lifetimes than
trait definition"* error). This is load-bearing: a consumer moves the stream
into a per-request task, so it has to be `'static`. Clone or `Arc` whatever the
walk needs into it, as the reader is cloned into the closure above.

Neither constrains **how** you chunk. Boundaries carry no meaning to a consumer,
so a byte budget rather than a block count, or a ramp on the first few chunks to
keep latency-to-first-byte low, is yours to choose and needs no port change.
This backend uses a flat `BLOCKS_PER_READ_TRANSACTION = 1024`, which is a
starting point rather than a considered answer: it bounds how long one read
transaction is held, but peak memory per in-flight request is 1024 *decoded*
blocks regardless of how dense they are.

## Three things a second implementer will hit

Recorded because they are mechanical and none is a trait today:

- `DbWrite::write_blocks_to_height` is generic per method, and the backend
  surface is RPITIT throughout. Neither is `dyn`-safe, so a second backend is a
  generic parameter threaded through, or an object-safe façade.
- `FinalisedSource<T>` is a closed `V1 | Ephemeral` enum, matched exhaustively
  in `init_or_take_ephemeral`, `update_ephemeral_db_height` and
  `primary_is_ephemeral`. That enum is the real second-adapter seam.
- Every read goes through `tokio::task::block_in_place`, which converts a
  runtime worker into a blocking thread for the duration and gives no bound on
  how many are converted at once. `spawn_blocking` has a bounded pool and
  therefore backpressure. Worth measuring before a second backend inherits the
  choice.
