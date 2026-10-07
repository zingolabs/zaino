# zaino-index-transparent-address

The transparent-address index. Backs `GetAddressUtxos[Stream]`,
`GetTaddressBalance[Stream]` and `GetTaddressTransactions` (plus its deprecated
alias `GetTaddressTxids`) from two map tables in one
[`zaino_persistence`](../zaino-persistence/usage.md) store, each a pure
projection of a block:

```text
receives   addr(21) ‖ height u32 ‖ txid(32) ‖ vout u32   ->  value_zat u64        scope 21
spent      txid(32) ‖ vout u32                           ->  height u32, spending txid
```

Keys are big-endian, so byte order is key order. `receives` declares the
address as its scope: one address's history is one key range. `spent` is read
by point lookups only. `schema(network)` returns the declared `Schema`.

## Wiring

```rust
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressService};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine};
use zcash_transparent::address::TransparentAddress;

let schema = zaino_index_transparent_address::schema(network);
let index = TransparentAddressIndexWriter::new(DiskEngine::new(fs).open(&path, &schema)?, batch_bytes);
let service = TransparentAddressService::new(index.published().served(), network);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
let subscription = block_sink.subscribe(IndexKind::TransparentAddress.name(), queue_bytes);
tokio::spawn(index.run(subscription)); // infallible: returns at Shutdown

let address = TransparentAddress::PublicKeyHash(hash160);
let unspent = service.utxos(&address, start)?;              // start (inclusive) to the tip; oldest first
let balance = service.balance(&address)?;                   // Zatoshis
let touched = service.transactions(&address, start, end)?; // both inclusive; start <= end, asserted

// many addresses, one view and one batched spend lookup (answers in `addresses` order)
let per_address = service.utxos_of(&addresses, start)?;     // Vec<Vec<AddressUtxo>>
let balances = service.balances(&addresses)?;            // Vec<Zatoshis>
```

- Every method is synchronous and reads the store's mmapped pages: a transport
  runs it off its async workers (`zaino-grpc`'s scan lane).
- `with_max_rows(n)` (default `DEFAULT_MAX_ADDRESS_ROWS` = 100,000) bounds the
  receives one request walks, across all its addresses and both tiers. The walk
  stops past `n` (`MapRead::range`'s `limit`), and the request is
  `ServeError::TooManyRows { limit }`, never a short list or a partial balance.
  Every method walks the address's whole history, whatever range it asks about.
- A request's spend checks are batched: every received outpoint of every
  address in it resolves through one `MapRead::values` call on `spent`, not one
  probe per outpoint.

- Generic over the persistence port: `TransparentAddressIndexWriter<S: Store>`
  with `S::View: MapRead`, serving `ReadView<V>` /
  `TransparentAddressService<V>`; zainod picks `DiskEngine`.
- `TransparentAddressIndexWriter::run` is this index's own loop over its
  `zaino_sync::BlockSink` subscription (one `match` per `Step`). Services read
  what it publishes (`published().served()`), gated by
  `published().gate(..)`'s task.
- Fallible only at boot (the engine's `open` → `StoreError`); `new` and `run`
  are infallible: `run` panics on a failed commit
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.transparent_address.enabled`; `zaino-grpc`'s
  `with_transparent_address` claims the methods and pairs `transactions` with
  the validator's `GetTransaction` to return bytes.
- Queries take librustzcash's `TransparentAddress`; its `script()` is the
  exact locking script the rows were stored under, so `GetAddressUtxos`
  returns a script the index never stored.
- Storage keys are internal: exact-length, exact prefix/suffix P2PKH and P2SH
  outputs key by `[hash160][kind]`; anything else goes under one opaque key,
  still stored, never queryable.
- Heights arrive as `Height`, already range-checked and ordered by the caller
  (`zaino-grpc` parses them at the router).

## No lookups in the fold

A spend is recorded under its outpoint, which the block carries, not under the
spending address. There is no outpoint map, no UTXO set and nothing mutable:
`apply` is a projection and the durable side never deletes. Queries compose the
maps: scan `receives` for the address, probe `spent` per outpoint; unspent = the
probes that miss. Cost is `O(received)` per address. See
[`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §5.

`transactions(start, end)` (both inclusive) scans all of history (an in-range spend consumes an
output received at any height) and reports both receipts and spends in range.

## Applied vs finalised

Both are tips: the last height held, inclusive (`None` = nothing held).

| | covers | moved by |
|---|---|---|
| applied | held blocks, reaches the tip | a non-final `Apply`, a commit, a `Reorg` |
| `durable_tip()` | the committed store | a commit |

- One block = its `receives` and `spent` rows (one `Changes`, projected inline
  on the loop, no lookups). Storage tiers are `zaino_persistence::Tiered`
  ([§5](../../docs/design/persistence-engine.md#5-tiering)): a non-final block
  is applied (RAM, no I/O); a final one is staged, committed in bulk once a
  batch's bytes fill, and at the tip on every `Finalized`, on the blocking
  pool; the loop awaits it before the next step.
- `Reorg` drops every applied block; the store is not touched. A reorg and a
  restart are the same operation: re-apply from the durable tip. See
  [`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).
- Reads pin one `ReadView`: the held rows over the committed store, merged by
  key (a row sits in exactly one tier), answering up to the view's tip.
- `Tiered` asserts contiguous heights from genesis: a gap would leave spent
  outputs counted as balance, so there is no start-height option.

## Storage

- `new(store, batch_bytes)` takes a store opened with `schema(network)` at its
  committed tip. Layout, merges, checksums and crash recovery are the engine's
  ([`zaino-persistence`](../zaino-persistence/usage.md)); a foreign network or
  format is refused at open.
- Stored bytes decode through named functions in `key.rs` (`encode_receive` /
  `decode_receive`, `encode_spend` / `decode_spend`, keys via
  `OutPoint::encode`), each pinned by golden bytes. A stored row that fails to
  decode panics naming the invariant: it was sealed and checksummed by this code.
- `zainod verify` scrubs a directory with
  `DiskEngine::verify(path, &schema(network))`.

## Errors

| Case | Result | gRPC |
|---|---|---|
| not yet synced to the tip | `ServeError::Syncing` | `UNAVAILABLE` |
| unspent sum over the money supply (corrupt store) | `ServeError::SupplyExceeded` | `INTERNAL` |
| synced, nothing matched | `Ok`, empty | — |

`Syncing` is checked before any other validation and carries no progress. An
empty result is never an error: a gap-limit walk must tell "no transactions"
from "cannot tell".

`service.network()` = the index's network; `zaino-grpc` refuses an address
encoded for another network (`INVALID_ARGUMENT`) before it reaches the index.
