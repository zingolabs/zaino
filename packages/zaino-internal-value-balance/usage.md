# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`Fee`](../zaino-primitives/usage.md) and derives one `BlockFees` per block. Its
writer sends them into a `zaino_sync::FeeSink`, one per final step, which
compact-block's writer reads to fill `CompactTx.fee`; at the tip the NFS folds
this index first and hands compact-block's fold the fees directly.

The only term a block does not carry is what each transparent input spends, so
the index keeps the UTXO set and each block's fees on the
[persistence port](../zaino-persistence/usage.md) (zainod: `DiskEngine`):

```text
outputs   OutPoint::encode() = txid(32) ‖ vout u32   ->  value_zat u64      unspent only, point lookups
fees      record h = block h's fees: per tx, tag u8 (0 coinbase, 1 paid) ‖ value_zat u64
```

Keys and values are big-endian. `outputs` declares `deletes()`: a block inserts
the outputs it leaves unspent and removes the prevouts it spends, and an output
created and spent in one block writes neither row
([lsm-deletes.md](../../docs/design/lsm-deletes.md)). Since a spent prevout is
gone, a block's fees cannot be re-derived later, so each block's fees are stored
in `fees` and read back for a held height.
`TABLES` + `FORMAT` declare the store; `zainod verify` checks the directory
against `Schema::new(IndexKind::ValueBalance, FORMAT, network, TABLES)`
(`PersistenceEngine::verify`).

## Wiring

```rust
use zaino_internal_value_balance::{ValueBalanceIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};
use zaino_sync::FeeSink;

let mut fee_sink = FeeSink::new();
let for_compact = fee_sink.subscribe("compact_block", queue); // before `run` takes the sink
let schema = Schema::new(IndexKind::ValueBalance, FORMAT, network, TABLES);
let writer = ValueBalanceIndexWriter::new(DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?);
let handle = writer.handle();               // also CompactBlockIndexWriter::new's fee source
let blocks = follower.subscribe(IndexKind::ValueBalance, handle.tip(), queue);
nfs.add(IndexKind::ValueBalance, handle);   // before compact_block: its fees feed that fold
tokio::spawn(writer.run(blocks, fee_sink));
```

- Generic over the persistence port: `ValueBalanceIndexWriter<S: Store>` with
  `S::View: SequenceRead + MapRead`; zainod picks `DiskEngine`. zainod enables it
  with `index.compact_block` (its directory beside compact-block's).
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
let parent = ValueBalanceReader::new(view);           // any V: SequenceRead + MapRead
let mut out = store.changes(block.at());              // or the parent layer's `changes`
let fees = fold(&parent, &block, &mut out)?;          // Result<BlockFees, FoldError>
let stored = parent.block_fees(&block)?;              // a held block's fees, read back
```

- `fold(parent, block, out)` is the index's whole state transition (in
  `writer.rs`, beside the writer loop): into `out` go the outputs the block
  leaves unspent, a removal of each prevout it spends, and its fees record; its
  `BlockFees` is returned. Every prevout resolves from an earlier transaction of
  the block or through `parent`. `parent` must hold exactly the block's parent
  (genesis: empty) and `out` must be opened for the block, else a panic naming
  the index.
- `ValueBalanceReader::block_fees(block)` = the `BlockFees` stored when the
  block was folded, for a block at or below the reader's tip. The writer reads
  held heights through it, and so does the NFS when compact-block folds a block
  value-balance already holds. A height with no record is
  `StoredFeesMissing`, and a record that is malformed or not one fee per
  transaction of `block` is `StoredFeesUnreadable`.
- The writer folds a run's blocks at once (`fold_run`, crate-internal, one
  delta per block, each then `zaino_sync::apply`d), transaction by transaction
  in chain order: each input is spent out of the run's unspent set, then the
  transaction's outputs go in. Every prevout from outside the run is asked in
  one `MapRead::values` call. A sandblast transaction spends thousands of
  outputs, and one random lookup each is one cold page fault each. Spending an
  output twice, or one that only a later transaction creates, is
  `MissingPrevout`, as it would be alone.
- `FoldError` names the block and transaction: `MissingPrevout` (not an
  unspent output of the index: it runs from genesis, so a double spend, a
  foreign directory or a bug, never a gap to work around), `NegativeFee`,
  `ValueOverflow` (below); and the block for the two stored-fees errors.

## Resolved per run

Each run (`Subscription::next_run`, queued steps to the queue's budget) folds
the blocks it does not hold as one `fold_run` onto `staged()` on the blocking
pool and applies them. Held ones (a restart's resend) have their fees read back
from `fees`, with no rows. Every
step's fees go out, held ones first. Resolving at commit time instead would
deadlock, since compact-block waits on fees step by step while a commit waits
for a whole batch.

| Step                               | Rows           | Fees on the sink                        |
| ---------------------------------- | -------------- | --------------------------------------- |
| held (a restart, this index ahead) | already stored | read back (compact-block may be behind) |
| new                                | applied        | folded                                  |

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
