# zaino-internal-value-balance

The value-balance index. Serves no RPC itself: it resolves every transaction's
[`Fee`](../zaino-primitives/usage.md) and derives one `BlockFees` per block,
which its follower forwards into a `zaino_sync::FeeSink` for downstream
indexes. The compact-block index reads it, in lockstep with its own
block stream, to fill `CompactTx.fee`.

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
use zaino_sync::{FeeSink, IndexFollower};

let mut fee_sink = FeeSink::new("fees");
let for_compact = fee_sink.subscribe("compact_block", queue); // before `publishing`
let writer = ValueBalanceIndexWriter::open(fs, &path, network)?;
let subscription = block_sink.subscribe(NAME, queue);
let follower =
    IndexFollower::new(writer, subscription, tips, batch_bytes, depth).publishing(fee_sink);
```

- `ValueBalanceIndexWriter` implements `zaino_sync::IndexWriter<Input = Block>`
  and `zaino_sync::Derives<Item = BlockFees>`, subscribed to the
  `BlockSink` like any index.
- The follower forwards every step it follows into the sink, 1:1, `Shutdown`
  last (clean stop and failure alike): the stream is the block stream's, step
  for step, from the same start. A consumer reads it beside its own block
  subscription through `zaino_sync::Zip`.

## Resolved per delivered run

`deliver` records the run's outputs; `derive` resolves each block's inputs
against those and everything recorded before, for every block: bulk, replay
and tip alike. Resolving at commit time instead would deadlock, since the
consumer waits on fees block by block while a commit waits for a whole
batch. `apply` only moves the applied tip (last applied height, inclusive); `finalize` writes the
outputs `deliver` recorded; `reset` drops them.

A block's prevouts not held in memory are resolved in one
`Snapshot::get_many` (sorted, parallel, newest segment first). A sandblast
transaction spends thousands of outputs, and one random lookup each is one cold
page fault each.

| Height delivered | Outputs | Derived + forwarded |
|---|---|---|
| at or below this index's durable tip | already stored | yes (a consumer behind this index pairs it) |
| above it | recorded (`Pending`) | yes |

A spend of an output the index never recorded is fatal
(`IndexWriterError::MissingPrevout`): the index runs from genesis, so it means
a foreign directory or a bug, never a gap to work around.

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
  be nonnegative"), so it is fatal (`IndexWriterError::NegativeFee`), never
  clamped. So is a sum past the money supply (`ValueOverflow`, ZIP 209).

Mempool fees do not come from here:
the validator lists them (`getrawmempool true`), since it resolved those
prevouts itself when admitting the transaction.
