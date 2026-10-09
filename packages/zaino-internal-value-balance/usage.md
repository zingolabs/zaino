# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`Fee`](../zaino-primitives/usage.md) and derives one `BlockFees` per block. Its
writer sends them into a `zaino_sync::FeeSink`, one per final step, which
compact-block's writer reads to fill `CompactTx.fee`; at the tip the NFS folds
this index first and hands compact-block's fold the fees directly.

The only term a block does not carry is what each transparent input spends, so
the index keeps one map on the
[persistence port](../zaino-persistence/usage.md) (zainod: `DiskEngine`):

```text
outputs   OutPoint::encode() = txid(32) ‖ vout u32   ->  value_zat u64      point lookups only
```

Keys and values are big-endian. Every transparent output is kept, spent or not
(Shape B, insert only), so any height re-resolves identically.
`TABLES` + `FORMAT` declare the store; `zainod verify` checks the directory
against `Schema::new(IndexKind::ValueBalance, FORMAT, network, TABLES)`
(`PersistenceEngine::verify`).

## Wiring

```rust
use zaino_internal_value_balance::{ValueBalanceIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};
use zaino_sync::FeeSink;

let mut fee_sink = FeeSink::new("fees");
let for_compact = fee_sink.subscribe("compact_block", queue); // before `run` takes the sink
let schema = Schema::new(IndexKind::ValueBalance, FORMAT, network, TABLES);
let writer = ValueBalanceIndexWriter::new(DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?);
let handle = writer.handle();               // also CompactBlockIndexWriter::new's fee source
let blocks = follower.subscribe(IndexKind::ValueBalance, handle.tip(), queue);
nfs.add(IndexKind::ValueBalance, handle);   // before compact_block: its fees feed that fold
tokio::spawn(writer.run(blocks, fee_sink));
```

- Generic over the persistence port: `ValueBalanceIndexWriter<S: Store>` with
  `S::View: MapRead`; zainod picks `DiskEngine`. zainod enables it with
  `index.compact_block` (its directory beside compact-block's).
- `run` follows the final stream run by run
  ([the writer shape](../zaino-sync/usage.md#writer-loop)), then ends the fee
  sink. Fallible only at boot (the engine's `open` → `StoreError`); a failed
  commit or an unresolvable fee panics
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- One `BlockFees` per step goes into the fee sink, held heights included. On a
  panic the fee sink drops without `Shutdown`, so compact-block panics on its
  next pop.
- `handle()` = the `IndexHandle` the NFS reads (no route reads this index; the
  NFS's folds read it through `ValueBalanceReader`).

## Folding

```rust,ignore
let parent = ValueBalanceReader::new(view);           // any V: MapRead
let mut out = store.changes(block.at());              // or the parent layer's `changes`
let fees = fold(&parent, &block, &mut out)?;          // Result<BlockFees, FoldError>
let paid = fees(&parent, &[&block])?;                 // fees alone: no rows, any later parent
```

- `fold(parent, block, out)` is the index's whole state transition (in
  `writer.rs`, beside the writer loop): the block's outputs into `out` (one
  `outputs` row each) and its `BlockFees` returned, every prevout resolved from
  the block itself or through `parent`. `parent` must hold exactly the block's
  parent (genesis: empty) and `out` must be opened for the block, else a panic
  naming the index.
- `fees(parent, blocks)` = a run's `BlockFees` alone, no rows: `parent` = any
  state at or past the first block's parent (the map is insert only, so a later
  state resolves the same blocks identically). The writer re-folds held heights
  through it; the NFS tests price compact-block's fold with it.
- `ValueBalanceReader<V>` is generic over any `V: MapRead`; its reads are
  internal (fees are the only consumer).
- The writer folds a run's blocks at once (`fold_run`, crate-internal, one
  delta per block, each then `zaino_sync::apply`d): block `k`
  resolves against `parent` plus the outputs of blocks `0..=k`, and every
  prevout from outside the run is asked in one `MapRead::values` call. A
  sandblast transaction spends thousands of outputs, and one random lookup each
  is one cold page fault each. A block spending an output that only a later
  block of the run creates is `MissingPrevout`, as it would be alone.
- `FoldError` names the block and transaction: `MissingPrevout` (an output the
  index never recorded: it runs from genesis, so a foreign directory or a bug,
  never a gap to work around), `NegativeFee`, `ValueOverflow` (below).

## Resolved per run

Each run (`Subscription::next_run`, queued steps to the queue's budget) folds
the blocks it does not hold as one `fold_run` onto `staged()` on the blocking
pool and applies them; held ones (a restart's resend) are priced by `fees` with
no rows. Every
step's fees go out, held ones first. Resolving at commit time instead would
deadlock, since compact-block waits on fees step by step while a commit waits
for a whole batch.

| Step | Outputs | Fees on the sink |
|---|---|---|
| held (a restart, this index ahead) | already stored | yes (compact-block may be behind) |
| new | applied | yes |

A fold error panics the writer (`value_balance index: ` + the `FoldError`).

## Fees

A transaction's fee is the value it leaves in the transparent transaction
value pool
([protocol §3.4](https://zips.z.cash/protocol/protocol.pdf#transactions)):

```text
fee = Σ transparent inputs − Σ transparent outputs
    + Sprout Σ(vpub_new − vpub_old)   (§4.12)
    + valueBalanceSapling             (§4.13)
    + valueBalanceOrchard             (§4.14)
    + valueBalanceIronwood            (§4.14, ZIP 229)
```

These are the same terms as librustzcash's `fee_paid`.

- A coinbase transaction (decoded as `TransparentData::coinbase`) pays no fee
  (§3.11), so it is `Fee::Coinbase`, never a computed sum.
- A negative sum is consensus-invalid for any other transaction (§3.4: "MUST
  be nonnegative"), so it is `FoldError::NegativeFee` (the loop panics) and is
  never clamped. So does a sum past the money supply (`ValueOverflow`, ZIP 209).

Mempool fees do not come from here:
the validator lists them (`getrawmempool true`), since it resolved those
prevouts itself when admitting the transaction.
