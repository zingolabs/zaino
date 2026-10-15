# zaino-index-utxo-set

The utxo-set index. It serves no RPC itself. It resolves every mined
transaction's [`Fee`](../zaino-primitives/usage.md) so that compact-block can
fill `CompactTx.fee`, and it is the UTXO set zainod's chain view reads.

## What it stores

A fee needs the value of everything a transaction's transparent inputs spend,
and that is the one term a block does not carry: an input names only the
outpoint it spends. Every other term is in the transaction. So this index keeps
the UTXO set on the [persistence port](../zaino-persistence/usage.md) (zainod:
`DiskEngine`):

```text
outputs   txid(32) ‖ vout u32 BE   ->  value_zat u64 LE      unspent only, point lookups
fees      record h = block h's fees: per tx, tag u8 (0 coinbase, 1 paid) ‖ value_zat u64 LE
```

`outputs` declares `deletes()`: a block inserts
the outputs it leaves unspent and removes the prevouts it spends, and an output
created and spent in one block writes neither row
([zaino-persistence: Deletes](../zaino-persistence/usage.md#deletes)). Since a spent prevout is
gone, a block's fees cannot be re-derived later, so each block's fees are stored
in `fees` and read back for a held height.

- `outputs` is the only map in Zaino that deletes. Its deletes stay simple: an
  outpoint is inserted once and removed at most once, so a removal cancels its
  insert in whichever order the engine merges them.
- Keeping only unspent outputs keeps the set small enough to stay in the page
  cache. Every transparent input of every block probes it.
- Reorgs never reach storage: the NFS holds a non-final block's removals in
  memory, like its inserts.
- The index folds from genesis, so a missing prevout is never a gap to work
  around. It is a double spend, a foreign directory or a bug.

## Wiring

```rust,ignore
use zaino_index_utxo_set::{UtxoSetIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};
use zaino_sync::FeeSink;

let mut fee_sink = FeeSink::new();
let for_compact = fee_sink.subscribe("compact_block", queue); // before `run` takes the sink
let schema = Schema::new(IndexKind::UtxoSet, FORMAT, network, TABLES);
let writer = UtxoSetIndexWriter::new(DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?);
let handle = writer.handle();
let blocks = follower.subscribe(IndexKind::UtxoSet, handle.view().tip(), queue);
nfs.add(IndexKind::UtxoSet, handle);
tokio::spawn(writer.run(blocks, fee_sink));
```

- Generic over the persistence port: `UtxoSetIndexWriter<S: Store>` with
  `S::View: SequenceRead + MapRead`. zainod enables it with
  `index.compact_block`, its directory beside compact-block's.
- `run` follows the final stream ([the writer shape](../zaino-sync/usage.md#writer-loop))
  and sends one `BlockFees` per step into the fee sink, then ends the sink.
- Fallible only at boot (`StoreError` from `open`). A failed commit or an
  unresolvable fee panics
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)). The fee sink then
  drops without `Shutdown`, so compact-block panics on its next pop.
- `zainod verify` checks the directory against
  `Schema::new(IndexKind::UtxoSet, FORMAT, network, TABLES)`.

## Readers

No route reads this index. Its readers are:

- compact-block, through the fee sink in bulk, and at the tip through the NFS,
  which folds this index first and hands compact-block's fold the fees.
- zainod's `ServedUtxos`, which calls `UtxoSetReader::values(outpoints)` at
  the served tip so the chain view can price a peer-quorum mempool admission.

## Folding

```rust,ignore
let parent = UtxoSetReader::new(view);   // any V: SequenceRead + MapRead
let mut out = store.changes(block.at());      // or the parent layer's `changes`
let fees = fold(&parent, &block, &mut out)?;  // Result<BlockFees, FoldError>
let stored = parent.block_fees(&block)?;      // a held block's fees, read back
```

- `fold` writes the outputs the block leaves unspent, a removal per prevout it
  spends, and its fees record, and returns its `BlockFees`. Each prevout
  resolves from an earlier transaction of the block or through `parent`.
  `parent` must hold exactly the block's parent (genesis: empty) and `out` must
  be opened for the block, else a panic naming the index.
- The writer folds a run at once (crate-internal `fold_run`), transaction by
  transaction in chain order: inputs are spent out of the run's unspent set,
  then outputs go in. Every prevout from outside the run is fetched in one
  `MapRead::values` call, because a sandblast transaction spends thousands of
  outputs and one lookup each would be one cold page fault each.
- A held block (a restart's resend) writes no rows. Its fees are read back with
  `block_fees`, and every step's fees still go to the sink, because
  compact-block may be behind this index. Fees are resolved per run, not at
  commit time: compact-block waits on fees step by step while a commit waits
  for a whole batch, so resolving at commit would deadlock.
- `FoldError` names the block and transaction: `MissingPrevout` (spent twice,
  or created only by a later transaction), `NegativeFee`, `ValueOverflow`, and
  for read-back fees `StoredFeesMissing` / `StoredFeesUnreadable`.

## Fees

A transaction's fee is the value it leaves in the transparent transaction value
pool ([protocol §3.4](https://zips.z.cash/protocol/protocol.pdf#transactions)),
the same terms as librustzcash's `fee_paid`:

```text
fee = Σ transparent inputs − Σ transparent outputs
    + Sprout Σ(vpub_new − vpub_old)   (§4.12)
    + valueBalanceSapling             (§4.13)
    + valueBalanceOrchard             (§4.14)
    + valueBalanceIronwood            (§4.14, ZIP 229)
```

- A coinbase transaction pays no fee (§3.11), so it is `Fee::Coinbase`, never a
  computed sum.
- A negative sum is consensus-invalid for any other transaction (§3.4), so it
  is `FoldError::NegativeFee`, never clamped. A sum past the money supply is
  `ValueOverflow` (ZIP 209).
- Mempool fees do not come from this fold, since an unconfirmed transaction
  may spend another unconfirmed one. A trusted validator's
  `getrawmempool true` listing carries the fee.
