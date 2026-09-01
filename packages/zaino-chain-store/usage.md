# `zaino-chain-store` — usage

The domain half of the finalised-state subsystem: vocabulary and ports for
everything below the reorg seam. No runtime, no storage, no `zebra-chain`, no
`tonic`. The LMDB implementation is
[`zaino-chain-store-zainodb`](../zaino-chain-store-zainodb/usage.md).

Depend on this crate to *name* what a store answers. Depend on an
implementation only if you are the one starting it.

```rust
use zaino_chain_store::{ChainStoreReader, StoredBlockRead};

/// Works against any store that can serve stored blocks, and knows nothing
/// about how one is stored.
async fn tip_block<R: StoredBlockRead>(reader: &R) -> Option<StoredBlock> {
    let tip = reader.watermark().tip?;
    reader.blocks_chunk(tip.height, tip.height).await.ok()?.pop()
}
```

## Bound on the capability, not on the store

Only `ChainStoreReader` is universal. Compact blocks, transaction positions,
spent outputs, the txout set and address history are each their own trait.

Write `R: ChainStoreReader + TransactionIndex` and you have said exactly what
you use; a store that cannot serve transaction positions fails to compile
against you rather than failing at runtime. A single fat trait would force every
implementation to stub what it cannot do, and stubs are where
`Err(Unsupported)` at 3am comes from.

Where absence *cannot* be a compile-time fact it is a runtime one:
`capabilities()` exists because a store on an older schema genuinely lacks an
index until it has migrated. That is a fact about a database, not a type.

The two are tied together by a per-port capability *carrier* trait. Each read
port `X` has a sibling `XCapability: X` — `TxOutSetIndexCapability`,
`SpentOutputIndexCapability`, and so on — carrying the one capability that port
answers for:

```rust
// The value lives on the carrier, fixed by its sole blanket impl.
<Self as TxOutSetIndexCapability>::CAPABILITY   // == StoreCapability::TxOutSet
```

An implementation assembles its advertised set from those rather than by
choosing variants:

```rust
// Only compiles for a capability whose port `Self` actually implements.
StoreCapabilities::new([
    <Self as ChainStoreReaderCapability>::CAPABILITY,
    <Self as TxOutSetIndexCapability>::CAPABILITY,
])
```

The value cannot be restated by a backend. The carrier's `CAPABILITY` is set by
one blanket impl over every `T: X`, and because `X` is a supertrait of the
carrier the blanket impl is the *only* impl that can exist: a manual
`impl TxOutSetIndexCapability for MyStore` overlaps it (coherence rejects it,
E0119), and one for a type that does not implement the port fails the supertrait
bound. So the port↔capability pairing is a sealed fact, not a defaulted const an
implementor can override with the wrong variant.

Do not hand-write the variant either. Naming `StoreCapability::TxOutSet`
directly compiles whether or not you implement `TxOutSetIndex`, which is how a
store ends up advertising an index it cannot serve.

## The watermark is the boundary, and a read past it is not a miss

`ChainStoreError::AboveWatermark` means the height is not this store's to answer.
The block very likely exists — in the chain head. Treating it as absence is the
single most likely way to serve a wrong answer through this crate.

```rust
match reader.blocks_chunk(start, end).await {
    Err(ChainStoreError::AboveWatermark { watermark, .. }) => {
        // Ask the chain head for `(watermark, end]`, then concatenate.
    }
    // ...
}
```

Pin the watermark **once** for a request that spans the seam and derive both
sides from that one value. Re-reading it between the two halves lets the store
advance in the middle and produces a gap or an overlap.

```rust
// One read of the watermark, two derived ranges. Re-reading `watermark()` for
// the second half is the bug this shape exists to prevent: the store can
// commit a block between the two calls, and then either you ask both halves
// for the same height or neither.
let watermark = reader.watermark();

let (finalised, recent) = match watermark.tip {
    Some(tip) if tip.height >= end => (Some((start, end)), None),
    Some(tip) if tip.height >= start => (Some((start, tip.height)), Some((tip.height + 1, end))),
    // Empty store, or the whole request is above the seam.
    _ => (None, Some((start, end))),
};
```

Read `provenance` before you rely on `tip` as a coverage bound — see below.

### The boundary only applies to durable answers

Read `provenance` before you read `tip`. A store whose provenance is
`Passthrough` is answering from the validator rather than from what it holds, so
its durable tip is not a limit on what it can answer, and it will not refuse
above it. That is the state a store is in for the whole of a long initial
build — precisely when a node depends on it to stay useful — so a consumer that
treated `AboveWatermark` as the only way a read can be out of range would be
right about a settled store and wrong about a building one.

What the watermark still tells you in that state is what the store *holds*,
which is what a consumer deciding whether to trust it as a durable record wants.
The two questions are different, and `provenance` is which one you are asking.

## What a failure means, and what it carries

`ChainStoreError` distinguishes five conditions, and the distinctions are the
point — a caller that collapses them either retries what cannot succeed or
reports a healthy store as broken.

| Variant | Means | What a caller does |
| --- | --- | --- |
| `NotReady` | still opening | retry; it resolves itself |
| `AboveWatermark` | not this half's to answer | ask the chain head |
| `InvalidRange` | start above end | fix the request |
| `Unavailable(capability)` | this deployment does not build that index | route elsewhere; retrying never helps |
| `MissingRow` | an index points at a row that is not there | the store is damaged |
| `CorruptRow` | the row *is* there, and holds a value that cannot be read | the store is damaged |
| `Backend` | the storage engine failed | opaque; do not branch on it |

`MissingRow` and `CorruptRow` are both corruption but not the same repair: a
dangling index entry is rebuilt from the rows it references, while a corrupt
value means that row has to be refetched and rewritten. Alert on them
separately.

`Backend` is opaque to *branching* — its message is for an operator and no
domain logic should read it — but not to *reading*. Every failure carries its
cause, so log the chain rather than the top line:

```rust
// `{e}` alone prints "chain store backend failed: reading block 42" and stops.
// The LMDB errno underneath is in `source()`.
tracing::error!(error = &error as &dyn std::error::Error, "finalised read failed");
```

Construct these through the named methods rather than the variants —
`ChainStoreError::backend`, `backend_because`, `corrupt_row`,
`corrupt_row_because`, and `ChainStoreSourceError::unavailable`, `not_ready`,
`inconsistent_data`, `commit`, `commit_because`. The `_because` forms take the
message *and* the cause, because the useful message names what the store was
doing — which block, which height — and the cause's own `Display` does not.

Neither error type is `Clone`, `PartialEq` or `Eq`. Those derives would force
every cause back into a `String` and leave `source()` returning `None`, which is
the opposite of what `Backend` is for. Compare by matching the variant:

```rust
assert!(matches!(error, ChainStoreError::CorruptRow { .. }));
```

## Configuration is two halves, and the split is not arbitrary

`ChainStoreConfig` is what every store takes. An implementation pairs it with
its own type for the things a domain crate cannot name — for ZainoDB that is
`ZainoDbConfig`, carrying an LMDB budget and a `zebra-chain` network, and
nothing else:

```rust
FinalisedState::spawn(
    ChainStoreConfig::at_path("/var/lib/zaino"),
    ZainoDbConfig::new(network),
    source,
)
```

The rule for deciding which half a knob belongs in: ask whether a second
implementation would have the same question. Where the store lives, which
schema to target, and how it behaves when a build fails are the same question
for any store. A memory-map size is not.

Fields are private and read through accessors, and three of the four numeric
knobs are `NonZero` — the same shape as `MempoolConfig` and `ChainHeadConfig`.
The one that keeps its zero is `background_build_threshold`, because zero is
meaningful there (every build runs in the background); taking `NonZero` for
uniformity would have removed a real configuration.

Note what is *not* two fields: a store that holds nothing is one with no path,
not a path beside a flag. That pair used to exist, and nothing said which won
when they disagreed.

## A stored block is not a compact block

`StoredBlock.transactions` holds `StoredTx`, not `PreIndexCompactTx`: the
compact transaction *plus* the per-pool value balances an index persists beside
it. The compact protocol has no value balance, correctly — a wallet does not
need one — but a store does, and `StoredBlock` is the shape that crosses the
write boundary as well as the read one.

That is the rule to keep when adding a field here: whatever
`ChainStoreFreezeSink::freeze` needs in order to write a block must be
expressible in what `StoredBlockRead` yields, or a block read out of one store
and frozen into another loses it silently. Both directions decode, both hash,
and only the rows differ. The backend's port suite checks exactly this by
reading a chain out of one store and freezing it into an empty one.

## Chunks, not blocks

There is no `get_block(height)`, and that is deliberate. A single block is
`blocks_chunk(h, h)`. Naming a point read invites the pattern this port
replaced: one `begin_ro_txn` per height across a range, plus one channel send
per block.

Use `blocks_chunk` when you know the range is small and you want it in hand.
Use `blocks_stream` for anything client-facing: it walks one cursor and yields
a `Vec` per read transaction, so peak memory is chunk-sized rather than
range-sized.

The stream comes back opaque rather than boxed, so nothing allocates to hand it
across the port. Two things follow. A consumer pins it — `std::pin::pin!`, on
the stack, which costs nothing — and only one multiplexing several streams
through a combinator like `select_all` needs to box. An implementation returns
exactly one stream type per method: fold a branch such as "the whole range is
above the watermark" into the stream's own state rather than returning
`stream::empty()` from one arm.

Chunk boundaries are the implementation's to choose and carry no meaning. Sizing
them by bytes rather than by count, or ramping the first few so
latency-to-first-byte stays low, needs no port change.

### Consuming `blocks_stream`

```rust
use futures::StreamExt as _;
use zaino_chain_store::{ChainStoreError, StoredBlock, StoredBlockRead};
use zaino_primitives::types::Height;

async fn send_range<R: StoredBlockRead>(
    reader: &R,
    start: Height,
    end: Height,
    out: &tokio::sync::mpsc::Sender<StoredBlock>,
) -> Result<(), ChainStoreError> {
    let chunks = reader.blocks_stream(start, end).await?;

    // `pin!` because the stream is `!Unpin` — it is a state machine built from
    // an async block. This pins it to the stack; nothing is allocated.
    let mut chunks = std::pin::pin!(chunks);

    while let Some(chunk) = chunks.next().await {
        // One `Result` per chunk, not per block: a read transaction either
        // yields its whole batch or fails, and a failure ends the walk.
        for block in chunk? {
            if out.send(block).await.is_err() {
                // The receiver is gone. Returning drops the stream, which is
                // the cancellation path: the walk stops and the store's read
                // transaction closes. There is nothing to signal explicitly.
                return Ok(());
            }
        }
    }

    Ok(())
}
```

Three things that example is showing, all of which are easy to get wrong:

- **Pin it.** `.next()` needs `Pin<&mut Self>`. `std::pin::pin!` is a stack pin
  and costs nothing; `Box::pin` also works and allocates. Reach for the latter
  only when you must hold several streams in a collection or drive them through
  a combinator like `select_all`, which require `Unpin`.
- **The `Result` is per chunk.** `chunk?` propagates a failed read transaction.
  A chunk that arrives is whole; there is no partially-failed chunk.
- **Dropping is cancelling.** Backpressure is the `await` on `out.send`: the
  store reads the next chunk only when you ask for it, so a slow client slows
  the walk rather than buffering ahead of it.

Because the port forbids the stream borrowing the reader, it is `'static` and
can be moved into a task that outlives the frame that made it:

```rust
let chunks = reader.blocks_stream(start, end).await?;
tokio::spawn(async move {
    let mut chunks = std::pin::pin!(chunks);
    while let Some(chunk) = chunks.next().await {
        // ...
    }
});
```

Note what is *not* here: no `Arc`. A stream is a cursor, and `poll_next` needs
unique access to advance it — an `Arc` cannot be polled, and wrapping one in a
`Mutex` would serialise the readers you were trying to run concurrently. Share
the *store* (implementations are cheap to clone and sit behind an `Arc`
internally); give each request its own stream.

### Ranges are ascending, and reversing has a right way

Ranges are **ascending and hole-intolerant**. A gap in the heights is an error,
not a skip — silently truncating a wallet's sync is worse than failing it.

There is no descending read, deliberately: walking backwards would double every
range path in every implementation to serve one caller. Reverse above the port —
but reverse *per chunk*, walking the range downwards, rather than collecting it
and reversing the result:

```rust
// Right: peak memory is one chunk, whichever direction you serve.
let mut top = end;
loop {
    let bottom = top.saturating_sub(CHUNK - 1).max(start);
    let mut blocks = reader.blocks_chunk(bottom, top).await?;
    blocks.reverse();
    yield_all(blocks).await;
    if bottom <= start { break; }
    top = bottom - 1;
}

// Wrong: buffers the whole range before emitting anything.
let mut all: Vec<_> = collect(reader.blocks_stream(start, end).await?).await?;
all.reverse();
```

The wrong version is correct and will pass a test over a hundred blocks. It
fails over a hundred thousand, on a machine that is also serving everyone else.

## `PoolFilter` goes into the read

`CompactBlockRead` is separate from `StoredBlockRead` for one reason: the filter
selects which cursors open and which row families decode. Passing it in lets a
sapling-only wallet skip orchard, ironwood and the commitment-tree rows
entirely. Filtering the result afterwards costs the decode you were avoiding, on
every block.

Build one from `all()`, `none()` or `default()` — every shielded pool and no
transparent data, which is what a light wallet asks for — then narrow or widen
with `with_pool` and `with_transparent`. It is `Copy`, so passing it into a
chunked walk costs nothing.

Inspect it with `includes(pool)` and `includes_transparent()`. There is no
per-pool accessor on purpose: the shielded pools are a set, and a new pool is
one entry in `ShieldedPool::ALL` rather than a field, a constructor arm and a
reader added everywhere.

## The spend reads are batched, and carry what you were going to look up next

`outpoint_spenders`, `previous_outputs` and `transparent_outputs` take slices
because the call sites they replaced looped a singular form with one `await` per
input. Ask once:

```rust
// The answer is positional: `spenders[i]` is the spender of `outpoints[i]`,
// and `None` means "not spent within this store" — which is not the same as
// "unspent", because the spend may be in the chain head.
let spenders = reader.outpoint_spenders(&outpoints).await?;

for (outpoint, spender) in outpoints.iter().zip(spenders) {
    match spender {
        // `SpenderRef` carries the txid as well as the position. That is the
        // point: the store resolved it while the index was already open, so
        // there is no second lookup per spend here.
        Some(spender) => mark_spent(outpoint, spender.txid, spender.position),
        None => check_the_chain_head(outpoint),
    }
}
```

`unspent_output` is first-class rather than "fetch it, then check whether it is
spent" — two calls across two capability routes, one of which errored on
absence. That is what `gettxout` wants.

## The store never hands you consensus bytes

What is stored is a projection — the fields an index reads, not the bytes a
block hash commits to. A `StoredTxOut` carries a 20-byte address key and a
value; the locking script is not recoverable. `StoredAddress` can express
`NonStandard`, which `TransparentAddress` cannot, and that is why it exists.

Raw blocks and raw transactions come from the validator. No port here offers
them, so that nothing can mistake a store for a source of consensus data.

Two further limits worth knowing before you design against this crate:

- **The store cannot answer maturity questions.** Coinbase is special-cased on
  inputs — null prevouts are filtered — and no stored output carries a coinbase
  flag.
- **The address index and the txout set disagree about which outputs exist,
  by design.** The address index keys every output, including non-standard
  scripts; the txout set excludes unspendable ones per `is_unspendable`. Each
  port states which semantics it exposes; do not assume they agree.

## `txout_set` is a partial fold, not an answer

`TxOutSetIndex::txout_set` returns an accumulator over the finalised set. It is
completed with the chain head's blocks by whatever merges the two. Serving it
directly as `gettxoutsetinfo` reports the chain as of the watermark, which is
not the chain.

```rust
use zaino_chain_store::{is_unspendable, TxOutSetIndex};

let mut accumulator = reader.txout_set().await?;

for block in recent_blocks {
    for tx in block.transactions() {
        for (index, output) in tx.outputs().iter().enumerate() {
            // Membership is part of the commitment, not a local convention.
            // Use this rule, not one of your own, or the two halves of the
            // fold will mean different things.
            if is_unspendable(output) {
                continue;
            }
            accumulator.apply_added_output(&outpoint(tx, index), output)?;
        }
        for spent in tx.spent_outpoints() {
            accumulator.apply_removed_output(&spent, &prev_output(spent)?)?;
        }
    }
}
```

The fold is order-independent — the commitment is a multiset XOR — so blocks
may be applied in any order, and an output added and later removed leaves no
trace. What is *not* optional is `is_unspendable`: a consumer that filters
differently produces a different number for the same chain, and nothing
anywhere reports the disagreement.

The commitment lives in this crate rather than in an implementation because two
stores disagreeing about it would not fail — they would quietly mean different
things by the same number.

## Do not build on `StoreCapabilities`

It is interim wiring: the backend's internal routing model, one bit per storage
trait, surfaced so `ChainIndex` keeps working until the chain view lands. It is
storage-shaped where the layer above needs "what is answerable to height H" per
*domain* capability. Its replacement is planned; adding a consumer adds work to
that replacement.

Mechanically it is a `Copy` bit set: `new` takes any
`IntoIterator<Item = StoreCapability>` and absorbs duplicates, `contains` is a
mask test, and `iter` yields ascending. `StoreCapability::ALL` enumerates the
closed set, which is what a coherence check over the ports iterates rather than
listing variants a second time.
