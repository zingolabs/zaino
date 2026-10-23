# zaino-internal-txid-location

The txid → mined location index. When no validator answers `GetTransaction` or
`GetTaddressTransactions`, and the served mempool lacks the transaction, the route resolves a
mined transaction through it, fetches its block by hash from the peers, and checks that block
before serving the transaction's bytes. It answers no gRPC method of its own and stores no
transaction bytes: those stay with the validators and the network.

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

Wired by `zaino_nfs::writer` ([zaino-nfs wiring](../zaino-nfs/usage.md#wiring-nfs)). The
writer is the block-hash index's shape ([the writer shape](../zaino-sync/usage.md#writer-loop)).
zainod builds it only when `index.txid_location.enabled`. Disabled, a mined transaction no
validator answers is `UNAVAILABLE`.

## Serving

- A route reads `at.views().txid_location()?.mined(&txid)` → `Mined { block, index }`.
- A txid it does not hold is `UNAVAILABLE`: only a validator can say a transaction is absent.
- The peer's block bytes must be `Mined.block` (hash, coinbase height, merkle root) with `txid` at
  `Mined.index` (`zaino_source::mined_transaction`) before the transaction's bytes go out.

## Tests

- Golden bytes of a row (`by_txid.rs`).
- A writer over `SimFs`: every txid located at its block and position after a restart that resends
  a held block, a never-mined txid nowhere (`writer.rs`).
