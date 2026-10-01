# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`Fee`](../zaino-primitives/usage.md) and derives one `BlockFees` per block,
which its loop republishes into a `zaino_sync::FeeSink` for downstream
indexes. The compact-block index reads one fee step after each block step to
fill `CompactTx.fee`.

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
use zaino_sync::FeeSink;

let mut fee_sink = FeeSink::new("fees");
let for_compact = fee_sink.subscribe("compact_block", queue); // before `run` takes the sink
let index = ValueBalanceIndexWriter::open(fs, &path, network, batch_bytes)?;
let durable = index.durable_tip(); // for the producer's start and chain check
let published = index.published(); // tips + gate for metrics and status
let blocks = block_sink.subscribe(ValueBalanceIndexWriter::NAME, queue);
tokio::spawn(index.run(blocks, fee_sink));
```

- `run` = the index's own loop over its `BlockSink` subscription, through
  `Shutdown`. Fallible only at boot (`open` → `StoreError`). `run` is
  infallible: a failed commit or an unresolvable fee panics
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- Every step it follows goes into the fee sink, 1:1, from the same start, with
  `Shutdown` last. A consumer awaits one fee step after each of its own block
  steps. On a panic the fee sink drops without `Shutdown`, so the consumer
  panics on its next pop.
- `published()` carries `()` as its view (no service reads this index), plus
  its durable and applied tips.

## Resolved per delivered run

Each `Apply` pulls every `Apply` already queued, to `batch_bytes`, into one
run. The run's outputs go into `Pending`, then every block's inputs are
resolved against those and everything recorded before, for every block: bulk,
replay and tip alike. Resolving at commit time instead would deadlock, since
the consumer waits on fees block by block while a commit waits for a whole
batch. Applying a non-final block only moves the applied tip (last applied
height, inclusive); a commit writes the outputs the run recorded; a reorg
drops the non-final ones.

A block's prevouts not held in memory are resolved in one
`Snapshot::get_many` (sorted, parallel, newest segment first). A sandblast
transaction spends thousands of outputs, and one random lookup each is one cold
page fault each.

| Height delivered | Outputs | Derived + forwarded |
|---|---|---|
| at or below this index's durable tip | already stored | yes (a consumer behind this index pairs it) |
| above it | recorded (`Pending`) | yes |

A spend of an output the index never recorded panics
(`value_balance index: ` + `IndexWriterError::MissingPrevout`): the index runs
from genesis, so it means a foreign directory or a bug, never a gap to work
around.

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
  be nonnegative"), so it panics (`IndexWriterError::NegativeFee`) and is never
  clamped. So does a sum past the money supply (`ValueOverflow`, ZIP 209).

Mempool fees do not come from here:
the validator lists them (`getrawmempool true`), since it resolved those
prevouts itself when admitting the transaction.
