# zaino-internal-txid-location

The txid → mined location index. `GetTransaction` and `GetTaddressTransactions` resolve a mined
transaction through it, then fetch its block by hash from whichever member the balancer picks
(trusted first, then the peers) and check that block before serving the transaction's bytes. It
answers no gRPC method of its own and stores no transaction bytes: those stay with the
validators and the network.

## What it stores

One map on the [persistence port](../zaino-persistence/usage.md):

```text
by_txid   txid 32 (protocol byte order) → height u32 BE | index u32 BE | block hash 32
```

- Each transaction contributes one row: its block, and its position in it (coinbase = 0).
  `fold` reads nothing but the parent's tip.
- Insert only: txids are unique since genesis (BIP 34's coinbase height), and a finalized
  transaction never moves. Blocks above the durable tip are `zaino-nfs` layers.
- The block hash rides in the row, so the answering index owns the block it names: a request
  needs no other index to fetch it.

## Wiring

```rust,ignore
use zaino_internal_txid_location::{schema, TxidLocationIndexWriter, WRITE_BUFFER};

let writer = TxidLocationIndexWriter::new(engine.open(&path, &schema(network), WRITE_BUFFER)?);
let handle = writer.handle();
let blocks = follower.subscribe(IndexKind::TxidLocation, handle.durable_view().tip(), queue_bytes);
nfs.add(IndexKind::TxidLocation, handle);
tokio::spawn(writer.run(blocks));
```

The writer is the block-hash index's shape ([the writer shape](../zaino-sync/usage.md#writer-loop)).
zainod builds it only when `index.txid_location.enabled`. Disabled, `GetTransaction` and
`GetTaddressTransactions` answer `UNIMPLEMENTED` for a mined transaction.

## Serving

- A route reads `at.views().txid_location()?.mined(&txid)` → `Mined { block, index }`.
- A txid it does not hold, or one above the answering index's `IndexesAtBlock::answers_through`,
  is not mined in the served chain: the mempool answers it, else `NOT_FOUND`.
- The block is checked by `zaino_sync::fetch` (hash, merkle root, coinbase height) and the
  transaction's txid recomputed before its bytes go out.

## Tests

- Golden bytes of a row (`by_txid.rs`).
- A writer over `SimFs`: every txid located at its block and position after a restart that resends
  a held block, a never-mined txid nowhere (`writer.rs`).
