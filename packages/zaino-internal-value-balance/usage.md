# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`ValueBalance`](../zaino-primitives/usage.md) (what the transaction moves out
of each pool, whose sum is its fee) and publishes one `BlockValueBalances` per
block into a `zaino_sync::ValueBalanceSink` for downstream indexes. The
compact-block index reads it to fill `CompactTx.fee`.

The only term a block does not carry is what each transparent input spends, so
the index keeps one
[`zaino_persistence::lsm`](../zaino-persistence/usage.md#lsm-segments) set:

```text
outputs   txid(32) ‖ vout u32   ->  value_zat u64      probed, never scanned
```

Keys are big-endian. Every transparent output is kept, spent or not (Shape B,
append-only), so any height re-resolves identically.

## Wiring

```rust
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_sync::{IndexFollower, ValueBalanceSinkBuilder};

let mut balances = ValueBalanceSinkBuilder::new(depth);
let for_compact = balances.subscribe("compact_block", queue, compact_durable);
let writer = ValueBalanceIndexWriter::open(fs, &path, network, balances.seal())?;
let follower = IndexFollower::new(writer, blocks.subscribe(NAME, queue, durable), batch_bytes);

// downstream, once per block, in its own `deliver`
let balances = for_compact.balances_for(&block).await; // None = the publisher stopped
```

- `ValueBalanceIndexWriter` implements `zaino_sync::IndexWriter<Input = Block>`,
  subscribed to the `BlockSink` like any index.
- Seal the `ValueBalanceSink` after every consumer subscribes: it starts at
  their rearmost durable extent, and the writer publishes from there.
- A consumer pairs each block with its balances by height and hash
  (`Subscription::balances_for`), so after a reorg it skips items still
  queued from the losing branch.

## Everything happens in `deliver`

`deliver` records the block's outputs, resolves its inputs against those and
everything recorded before, and publishes, for every block: bulk, replay and
tip alike. Resolving at commit time instead would deadlock, since the consumer
waits on balances block by block while a commit waits for a whole batch.
`apply` only moves the nonfinalised extent; `finalize` writes the outputs
`deliver` recorded; `reset` drops them and resets the sink.

A block's prevouts not held in memory are resolved in one
`Snapshot::get_many` (sorted, parallel, newest segment first). A sandblast
transaction spends thousands of outputs, and one random lookup each is one cold
page fault each.

| Height delivered | Outputs | Published |
|---|---|---|
| inside this index's durable extent | already stored | if a consumer needs it |
| above it | recorded (`Pending`) | if a consumer needs it |

A spend of an output the index never recorded is fatal
(`IndexWriterError::MissingPrevout`): the index runs from genesis, so it means
a foreign directory or a bug, never a gap to work around.

## Fees

`ValueBalance::fee()` sums the transparent, Sprout, Sapling, Orchard and
Ironwood flows, the same terms as librustzcash's `fee_paid`. It is `None` for a
coinbase, whose sum is negative (issuance). Mempool fees do not come from here:
the validator lists them (`getrawmempool true`), since it resolved those
prevouts itself when admitting the transaction.
