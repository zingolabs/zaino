# `zaino-chain`

ChainView: one coherent read surface over the whole chain, composed from the
finalised store, the recent chain-head window, and the validator.

## The whole mechanism

Each provider covers a range of heights:

```text
store   [genesis, watermark]     durable; a single chain at that depth
head    [floor, tip]             the recent window, with competing branches
source  everything               the validator
```

A read asks **who covers this height**, and asks them. A range splits across
whichever providers cover it and stitches the answers in height order; a
single-height read is that with one segment. There is no second mechanism and no
per-capability routing table.

Where both tiers cover a height the **store wins**: one rule, and the durable
tier is authoritative for settled history.

The store starts at genesis by contract, and the head's window is sized to
overlap it, so in steady state the two meet. **While the store is still building
they do not, and the range between them is served by the validator.** That hole
is the normal state during sync, not a degraded corner — a read there is filled,
never refused.

## Chainwork crosses the seam

Chainwork is cumulative, so it needs an unbroken chain *below* a block. Only the
store has that — its coverage reaches genesis by contract — which is why a
validator-answered block carries none, and why nothing above an open hole does
either.

A chain-head block is the one case that recovers. The head measures work from
its own anchor, the parent of its window floor, and that anchor sits below the
window where the store eventually builds. So once the store has reached it:

```text
absolute(B) = chainwork(anchor) + work(B)
```

One addition, one store read, resolved once per snapshot and shared by every
read taken from it. Until the store reaches the anchor, `chainwork` is `None` —
there is no correct value to serve, and a plausible wrong one is worse than an
absent right one.

The consequence worth knowing: `chainwork` is `Some` across the finalised seam
in steady state, on one continuous scale, and goes `None` for the whole recent
window while the store is still catching up. Both are ordinary states, not
faults.

## Reading

```rust,ignore
use zaino_chain::{BlockId, ChainView as _, ChainViewSnapshot as _, CompactBlockRead as _};

let chain = view.snapshot();                                  // infallible, O(1)
let hash  = chain.block_hash(height).await?;
let block = chain.block(BlockId::Height(height)).await?;
```

Anything addressed by a block takes a [`BlockId`] — `Height` or `Hash` — rather
than appearing twice. That is how the chain is addressed (zebra takes a
`HashOrHeight`), and it halves the surface without removing anything.

A read is asked the way the caller asked it. A raw block wanted by height costs
one round trip, not a hash resolution and a fetch — and still works for a height
in a hole, where there is nothing local to resolve against.

## Ranges stream

```rust,ignore
let mut blocks = chain.stream_compact(start, end, pools);
while let Some(chunk) = blocks.try_next().await? {
    // chunk: Vec<CompactBlock>
}
```

The wallet-sync hot path, and what backs the `GetBlockRange` gRPC streaming
endpoint. Three properties matter at scale:

- **Chunks, not blocks.** Per-item overhead across hundreds of thousands of
  blocks dominates, and the store's own range ports are chunked for the same
  reason.
- **Sized by bytes, not count.** Block sizes vary by orders of magnitude across
  the chain, so a fixed count would make memory per client swing with them. The
  walk starts small — latency to first byte — and adapts toward the budget from
  what it has seen.
- **Lazy.** A stream does no work until polled, so a slow client applies
  backpressure by not reading. A producer task filling a channel would buffer
  per client instead, which with thousands of them is the difference between
  bounded and unbounded memory.

## Capabilities are traits, and optional ones are absent

The always-available reads — `BlockRead`, `CompactBlockRead`,
`TransactionRead`, `TreestateRead`, `ForkReconcile` — are bundled by
`ChainViewSnapshot`. `AddressRead`, `SpendRead` and `TxOutSetRead` are
implemented **only** where the providers can back them, so a store that builds
no spend index yields a view that does not have the trait:

```rust,ignore
fn serve(chain: impl ChainViewSnapshot + SpendRead) { .. }   // names what it needs
```

That is the point of splitting them: a capability the providers cannot support
must be absent, not present and always failing, and an impl can only be absent
if there is a trait for it to be absent from. `ServiceabilityManifest` reports
the same fact at runtime, derived from the same coverage the reads consult.

```rust,ignore
match view.serviceability().get(ChainCapability::SpendStatus) {
    Answerable::Absent        => // not built or not opted in; configuration might help
    Answerable::NotAnswerable => // nothing serviceable yet; waiting will help
    Answerable::ToHeight(h)   => // answerable for every height up to and including h
}
```

## Validator concurrency

Gap fills share one permit pool across every client, sized by
`ChainViewConfig::passthrough_permits` (default 512). Without it, concurrency is
the product of clients and per-client concurrency — 5,000 clients fetching 16
blocks each is 80,000 simultaneous requests — and nothing else in the stack caps
it.

Two things make it safe to size modestly:

- **Store reads are never gated.** The normal path, once caught up, never takes
  a permit at all.
- **A permit is held per block fetched, never for the length of a stream.** One
  client syncing a million blocks cannot starve anyone.

A client waiting on a permit is a parked task costing a few hundred bytes, so
thousands of clients queue inside Zaino rather than inside the validator.
`passthrough_per_request` (default 16) additionally stops one large range from
occupying the whole pool.

## Coherence

Every read goes through a snapshot, never the handle. A snapshot pins the chain
as of one tip and keeps answering from it while any clone lives — across reorgs
and across the tiers advancing underneath.

`snapshot()` is infallible and synchronous: the chain head hands back a
published snapshot infallibly, the watermark is an in-memory value, and a store
reader is cheap to clone. Every ingredient is present when the call is made, so
there is nothing to await and no failure to invent.

The watermark and the head snapshot are captured together, in one call. That is
the whole coherence mechanism: read apart, the watermark could claim the store
covers a height the pinned head still thinks is recent, and a read there would
be answered by a store holding it under a *different hash* than the caller's
view believes.

## Three outcomes, not two

`block_height` and `transaction_locations` distinguish **absent**, **on the best
chain**, and **on a retained competing branch**. Only the chain head can report
the third; below its window there is a single chain, so the store's
best-chain-only index is correct there rather than deficient.

`block_height` returns `None` for a branch hash — it answers about the chain the
caller is reading. `fork_point` is the branch question, and still finds it.

## Configuration

```rust,ignore
ChainViewComposer::new(store, head, source, ChainViewConfig::default())

ChainViewConfig::default().without_store()        // head + validator only
ChainViewConfig::default().without_passthrough()  // serve only what we hold
```

`without_store` replaces the ephemeral mode that used to live inside the store.
`without_passthrough` makes a hole unserviceable and disables the reads no tier
holds at all — raw transactions, treestates, address history.

## Bring your own store, your own head, or both

`ChainViewComposer` is generic over three *ports* — `ChainStoreService`,
`ChainHeadBlockService` and `ChainViewSource` — and names no adapter anywhere.
Implement those traits and the composer works unchanged. A different finalised
store, a different chain head, Zebra's read state in an ephemeral mode: none of
them requires writing a composition.

The suite is the standing proof rather than a claim. `FakeStore`, `FakeHead` and
`FakeSource` are an independent implementation of all three tiers — none of them
is a Zaino production adapter — and every composition test runs the real
composer over them. `MinimalStore` goes further: it implements `ChainStoreReader`
and the block ports but deliberately *not* `SpentOutputIndex` or `TxOutSetIndex`,
so a store that builds only some indexes is a case the compiler checks rather
than a case someone remembered.

Writing your own `ChainView` is possible too — that is why the ports are
separate from the composer — but it is for a consumer who wants different
*composition*, not one who merely has different storage.

## Offering less than the tiers can do

`ChainViewComposer::new` offers every capability its tiers can answer, which is
what every deployment in this workspace wants. A deployment that must withhold
one — a node that keeps a spend index but must not expose spend status — uses
the builder instead:

```rust,ignore
ChainViewComposer::builder(store, head, source)
    .with_config(config)
    .serving_spend_status()     // offered
    .build()                    // txout set and address history are not
```

It starts at `ServedCapabilities::CORE` — blocks, compact blocks, transactions,
treestate, subtree roots, chain tips — and takes the three optional capabilities
by name.

**Each `serving_*` method exists only when both tiers can supply its half.**
`serving_spend_status` requires `Store::Reader: SpentOutputIndex` and
`Head::Snapshot: ChainHeadTransactionService`; a composition missing either does
not have the method. So a capability cannot be advertised by a deployment that
cannot answer it — checked by the compiler, not discovered by a client.

**The manifest and the reads consult the same set.** A withheld capability is
reported `Absent` *and* refused as not serviceable. A manifest that said
`Absent` while the read answered anyway would be worse than no manifest: a
consumer trusting it routes around a capability the view is serving, and one
ignoring it gets data the operator meant to withhold.

**It is a floor, not a ceiling.** Naming a capability offers it; whether it is
answerable, and to what height, is still derived from live coverage and the
store's runtime index set. A deployment can offer spend status and have the
manifest report `NotAnswerable` while the store is still building.

## Driving the store: `spawn_sync`

Everything above reads. One method writes: `spawn_sync` starts a background task
that feeds the finalised store from the chain head's freeze stream, so the seam
between the two tiers closes itself.

```rust
let view = Arc::new(ChainViewComposer::new(store, head, source, config));
let sync = view.spawn_sync(cancel.clone());
```

**There is no separate catch-up phase, and that is the design.** The chain head
emits a block once it falls below the consensus seam; the store accepts one only
at `tip + 1`. An empty store handed a block from the middle of the chain
therefore answers `ChainStoreError::FreezeGap`, which carries the height to build
to — and building to it *is* the initial sync. A cold start and a chain head that
re-anchored after an outage take the same path, so that path is exercised on
every run rather than being a startup branch nothing reaches twice.

The repair runs once per batch, not until it succeeds. A second gap means the
chain moved while the build was running; the next batch reports it again with a
nearer target, and looping in place would block on a chain that is still moving.

**It is asked for, never assumed.** A deployment that drives its own store —
building it from a validator on its own schedule — must not also get this, or two
writers contend on one database and multiply memory rather than throughput. So
the store stays caller-driven and this call is the caller.

It is deliberately not a cargo feature. The bounds already do what a flag would,
and do it per deployment rather than per build: `spawn_sync` exists only where
`Store: ChainStoreIngest + ChainStoreFreezeSink` and `Head: ChainHeadFreezeEvents`,
so a composer whose providers cannot freeze does not have the method. A feature
would be worse on both counts — features are additive across a workspace, so one
crate enabling it turns it on for every other consumer.

**Losing blocks costs a fetch, not correctness.** The freeze stream is
best-effort by contract: it is a `broadcast`, so a slow consumer is told it
lagged rather than blocking the chain head, and a chain head that re-anchors
never emits what it skipped. Neither is handled specially, because a missed block
becomes a gap on the next freeze and the gap repairs itself.

`ChainViewSync::status()` reports `Syncing` while a gap is open, `Ready` once
freezes are landing, and `Offline` once the loop has stopped. This is separate
from the store's own status, which says whether the *database* is healthy;
this says whether anything is still feeding it. Cancelling the token stops the
loop, and so does dropping the handle — `shutdown()` additionally publishes
`Closing`, so the stop is observable rather than merely effective.

## Testing

The `testing` feature provides one `Chain` and three views of it, so coverage
shapes are explicit at the call site:

```rust,ignore
let chain = Chain::of_length(1201);
ChainViewComposer::new(
    FakeStore::covering(&chain, 100),        // built to 100
    FakeHead::covering(&chain, 1100, 1200),  // window near the tip
    Arc::new(FakeSource::over(&chain)),      // knows everything
    ChainViewConfig::default(),
);                                           // -> a hole at 101..=1099
```

`FakeSource` records every question, which is how the by-height/by-hash
assertions work — the returned value is identical either way, and only the call
log distinguishes them.

`tests/vectors.rs` additionally drives the composition with the checked-in
regtest chain from `zaino-chain-store-zainodb/testing` (dev-dependency only, so
`zebra-chain` never reaches the shipped `testing` feature). The programmatic
`Chain` proves coverage *shapes*; the vectors prove the stitched output is
*correct* — it caught fakes that were silently serving synthetic blocks.

These fakes are also the *second implementation* of the ports;
`zaino-chain-store-zainodb` and `zaino-chain-head-service` are the first. A port
with only one implementation is that implementation's surface with extra steps.
When a fake here becomes awkward to write, that is a signal about the port — the
branch-retention split in `FakeHeadSnapshot` came from exactly that.

## What a chain view is not

Chain-only. The mempool is not part of it and this crate does not depend on
`zaino-mempool`. Transaction broadcast, node and network information
(`getinfo`, `getpeerinfo`, `getmininginfo`, `getblockchaininfo`), RPC serving and
daemon lifecycle are above this layer and reach the validator through
`zaino-source` directly.
