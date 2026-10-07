# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`Fee`](../zaino-primitives/usage.md) and derives one `BlockFees` per block. In
bulk sync its writer sends them into a `zaino_sync::FeeSink`, one per unfolded
step, which compact-block reads to fill `CompactTx.fee`; at the tip the NFS
folds this index first and hands compact-block's fold the fees directly.

The only term a block does not carry is what each transparent input spends, so
the index keeps one map on the
[persistence port](../zaino-persistence/usage.md) (zainod: `DiskEngine`):

```text
outputs   OutPoint::encode() = txid(32) ‖ vout u32   ->  value_zat u64      point lookups only
```

Keys and values are big-endian. Every transparent output is kept, spent or not
(Shape B, insert only), so any height re-resolves identically.
`schema(network)` is the store's `Schema`; `zainod verify` checks the directory
against it (`PersistenceEngine::verify`).

## Wiring

```rust
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine};
use zaino_sync::FeeSink;

let mut fee_sink = FeeSink::new("fees");
let for_compact = fee_sink.subscribe("compact_block", queue); // before `run` takes the sink
let schema = zaino_internal_value_balance::schema(network);
let writer = ValueBalanceIndexWriter::new(DiskEngine::new(fs).open(&path, &schema)?, batch_bytes);
// subscribed before compact_block (the NFS asserts it): its fees feed compact-block's fold
let blocks = nfs.subscribe(IndexKind::ValueBalance, writer.committed(), queue);
tokio::spawn(writer.run(blocks, fee_sink));
```

- Generic over the persistence port: `ValueBalanceIndexWriter<S: Store>` with
  `S::View: MapRead`; zainod picks `DiskEngine`. zainod enables it with
  `index.compact_block` (its directory beside compact-block's).
- `run` follows the final stream through `zaino_sync::Committer`
  ([the writer shape](../zaino-sync/usage.md#committer)), then ends the fee
  sink. Fallible only at boot (the engine's `open` → `StoreError`); a failed
  commit or an unresolvable fee panics
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- One `BlockFees` per **unfolded** step goes into the fee sink, held heights
  included; folded steps send nothing. On a panic the fee sink drops without
  `Shutdown`, so compact-block panics on its next pop.
- `committed()` = the committed-view watch the NFS reads (no route reads this
  index; the NFS's folds read it through `ValueBalanceReader`).

## Folding

```rust,ignore
let parent = ValueBalanceReader::new(view, network); // any V: MapRead
let (changes, fees) = fold(&parent, &block)?;         // Result<_, FoldError>
```

- `fold(parent, block)` is the index's whole state transition, pure: the
  block's outputs as one `Changes` (one `outputs` row each) and its
  `BlockFees`, every prevout resolved from the block itself or through
  `parent`.
- `parent` = any state at or past the block's parent: the map is insert only,
  so a later state resolves the same block identically (a held height re-folds
  against the state past it).
- `ValueBalanceReader<V>` is generic over any `V: MapRead`; its reads are
  internal (fees are the only consumer).
- The writer folds a run's unfolded steps at once (`fold_run`, crate-internal): block
  `k` resolves against `parent` plus the outputs of blocks `0..=k`, and every
  prevout from outside the run is asked in one `MapRead::values` call. A
  sandblast transaction spends thousands of outputs, and one random lookup each
  is one cold page fault each. A block spending an output that only a later
  block of the run creates is `MissingPrevout`, as it would be alone.
- `FoldError` names the block and transaction: `MissingPrevout` (an output the
  index never recorded: it runs from genesis, so a foreign directory or a bug,
  never a gap to work around), `NegativeFee`, `ValueOverflow` (below).

## Resolved per run

Each run (`Committer::next`, queued steps to `batch_bytes`) folds its unfolded
steps as one `fold_run` onto `staged()` on the CPU pool. Each step it does not
hold is applied; every unfolded step's fees go out, held ones too. Resolving at
commit time instead would deadlock, since compact-block waits on fees step by
step while a commit waits for a whole batch.

| Step | Outputs | Fees on the sink |
|---|---|---|
| unfolded, held (a restart, this index ahead) | already stored | yes (compact-block may be behind) |
| unfolded, new | applied | yes |
| folded | applied as the NFS sent them | no |

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
