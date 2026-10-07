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
by point lookups only. `TABLES` declares both maps, `FORMAT` their layout
version.

## Wiring

```rust
use zaino_index_transparent_address::{TransparentAddressIndexWriter, FORMAT, TABLES};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};

let schema = Schema::new(IndexKind::TransparentAddress, FORMAT, network, TABLES);
let writer = TransparentAddressIndexWriter::new(DiskEngine::new(fs).open(&path, &schema)?, batch_bytes);
let blocks = nfs.subscribe(IndexKind::TransparentAddress, writer.committed(), queue_bytes);
tokio::spawn(writer.run(blocks)); // returns at Shutdown
```

- Generic over the persistence port: `TransparentAddressIndexWriter<S: Store>`
  with `S::View: MapRead`; zainod picks `DiskEngine`.
- `run` follows the final stream through `zaino_sync::Committer`
  ([the writer shape](../zaino-sync/usage.md#committer)): unfolded steps not
  held are folded onto `staged()` on the CPU pool, each into the delta
  `Run::apply` opened for it, folded steps applied as sent.
  `committed()` = the committed-view watch the NFS reads.
- Fallible only at boot (the engine's `open` → `StoreError`); `run` panics on a
  failed commit ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.transparent_address.enabled`; disabled =
  its methods `UNIMPLEMENTED`.

## Serving

```rust
use zcash_transparent::address::TransparentAddress;

// a route's: snap.views().transparent_address() (None = disabled)
let reader = reader.as_of(snap.tip().height).with_max_rows(max_address_rows);
let address = TransparentAddress::PublicKeyHash(hash160);
let unspent = reader.utxos(&address, start)?;              // start (inclusive) to the tip; oldest first
let balance = reader.balance(&address)?;                   // Zatoshis
let touched = reader.transactions(&address, start, end)?; // both inclusive; start <= end, asserted

// many addresses, one batched spend lookup (answers in `addresses` order)
let per_address = reader.utxos_of(&addresses, start)?;     // Vec<Vec<AddressUtxo>>
let balances = reader.balances(&addresses)?;               // Vec<Zatoshis>
```

- `as_of(tip)` hides rows above `tip`: a receive above it is unseen, a spend
  above it leaves the output unspent (a snapshot's view can sit above its tip
  during bulk sync). `new` reads as of the view's own tip.
- `with_max_rows(n)` (default `DEFAULT_MAX_ADDRESS_ROWS` = 100,000) bounds the
  receives one request walks, across all its addresses. The walk stops past `n`
  (`MapRead::range`'s `limit`), and the request is
  `ServeError::TooManyRows { limit }`, never a short list or a partial balance.
  Every method walks the address's whole history, whatever range it asks about.
- Every method is synchronous and reads mmapped pages: a transport runs it off
  its async workers (`zaino-grpc`'s scan lane).
- A request's spend checks are batched: every received outpoint of every
  address resolves through one `MapRead::values` call on `spent`.
- Queries take librustzcash's `TransparentAddress`; its `script()` is the exact
  locking script the rows were stored under.
- Storage keys are internal: exact-length, exact prefix/suffix P2PKH and P2SH
  outputs key by `[hash160][kind]`; anything else goes under one opaque key,
  still stored, never queryable.

## No lookups in the fold

```rust
use zaino_index_transparent_address::{fold, TransparentAddressReader};

let parent = TransparentAddressReader::new(view);  // any `V: MapRead` over these tables
let mut out = store.changes(block.at());           // or the parent layer's `changes`
fold(&parent, &block, &mut out);                    // its receives + spent rows into `out`
```

A spend is recorded under its outpoint, which the block carries, not under the
spending address. There is no outpoint map, no UTXO set and nothing mutable:
`fold` is a projection (the parent is read only for its tip: the block must
extend it, the delta must be opened for the block, else a panic naming the
index) and the durable side never deletes. Queries compose the maps through a
`TransparentAddressReader`: scan `receives` for the address, probe `spent` per
outpoint; unspent = the probes that miss. Cost is `O(received)` per address.
See [`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §5.

`transactions(start, end)` (both inclusive) scans all of history (an in-range
spend consumes an output received at any height) and reports both receipts and
spends in range.

Non-final blocks are `zaino-nfs` layers (rows keyed as the maps); a snapshot's
reader merges them over the committed store by key. A writer asserts
contiguous heights: a gap would leave spent outputs counted as balance.

## Storage

- `new(store, batch_bytes)` takes a store opened with `TABLES` at its
  committed tip. Layout, merges, checksums and crash recovery are the engine's
  ([`zaino-persistence`](../zaino-persistence/usage.md)); a foreign network or
  format is refused at open.
- Stored bytes decode through named functions in `key.rs` (`encode_receive` /
  `decode_receive`, `encode_spend` / `decode_spend`, keys via
  `OutPoint::encode`), each pinned by golden bytes. A stored row that fails to
  decode panics naming the invariant.
- `zainod verify` scrubs a directory with `DiskEngine::verify(path, &schema)`,
  `schema` = `Schema::new(IndexKind::TransparentAddress, FORMAT, network, TABLES)`.

## Errors

| Case | Result | gRPC |
|---|---|---|
| more receives than the request may walk | `ServeError::TooManyRows` | `RESOURCE_EXHAUSTED` |
| unspent sum over the money supply (corrupt store) | `ServeError::SupplyExceeded` | `INTERNAL` |
| nothing matched | `Ok`, empty | — |

An empty result is never an error: a gap-limit walk must tell "no
transactions" from "cannot tell". `zaino-grpc` refuses an address encoded for
another network (`INVALID_ARGUMENT`, the snapshot's `params().network`) before
it reaches the index.
