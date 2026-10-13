# zaino-index-transparent-address

The transparent-address index. It backs `GetAddressUtxos[Stream]`,
`GetTaddressBalance[Stream]` and `GetTaddressTransactions` (plus its deprecated
alias `GetTaddressTxids`).

## What it stores

Two maps in one [`zaino_persistence`](../zaino-persistence/usage.md) store:

```text
receives   addr(21) ‖ height u32 ‖ txid(32) ‖ vout u32   ->  value_zat u64        scope = addr
spent      txid(32) ‖ vout u32                           ->  height u32 ‖ spending txid(32)
```

Each block contributes one `receives` row per transparent output and one
`spent` row per transparent input. Nothing else is written, and nothing is ever
deleted.

The block itself dictates this shape. A transparent input carries only the
outpoint it spends, never the address or the value it spends. So the index does
not resolve a spend back to its address when it writes. It records the spend
under the outpoint it already has. `fold` is a pure projection of one block:
it does no lookups, keeps no outpoint → address map, no UTXO set, and no state
between blocks.

Reads put the two maps back together:

- **UTXOs**: range-scan `receives` for the address, then probe `spent` for
  every outpoint found, in one batched `MapRead::values` call. The misses are
  unspent.
- **Balance**: the sum of the UTXOs over all of history.
- **Transactions**: the txids that paid the address (from `receives`), plus the
  txids that spent those outputs (from the `spent` hits).

The cost is `O(received)` per address rather than `O(unspent)`. The gap only
shows for a heavily reused address, such as an exchange hot wallet, not for a
light wallet's mostly single-use receivers. In exchange, there is no carried
state, no deletes, and a reorg never touches disk.

### Keys

- Keys are big-endian, so byte order is key order and a range is a byte range.
- An address is keyed by its 21-byte `[hash160][kind]` tag itself, not by an
  interned id. Interning would add a dictionary to keep in step and an
  indirection on every read. It pays off only for addresses with several rows,
  and ZIP-320 TEX and librustzcash ephemeral receivers are single-use by
  construction.
- `receives` declares the address as its scope. One address's history is one
  key range, and the store's per-segment filter covers only that 21-byte
  prefix, so a scan skips every segment that never saw the address. The hash
  goes first because the filter shards on a key's first 8 bytes, which must be
  uniform.
- `spent` is only point-probed. An unspent output (the common case) misses in
  every segment, and the filter turns most of those misses into no read at all.
- Exact-length, exact prefix/suffix P2PKH and P2SH scripts key by
  `[hash160][kind]`. Anything else goes under one opaque key: it is still
  stored, but no address parses to it.

## Wiring

```rust,ignore
use zaino_index_transparent_address::{TransparentAddressIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};

let schema = Schema::new(IndexKind::TransparentAddress, FORMAT, network, TABLES);
let store = DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?;
let writer = TransparentAddressIndexWriter::new(store);
let handle = writer.handle();
let blocks = follower.subscribe(IndexKind::TransparentAddress, handle.tip(), queue_bytes);
nfs.add(IndexKind::TransparentAddress, handle);
tokio::spawn(writer.run(blocks)); // returns at Shutdown
```

- Generic over the persistence port: `TransparentAddressIndexWriter<S: Store>`
  with `S::View: MapRead`. zainod picks `DiskEngine`.
- `run` follows the final stream ([the writer shape](../zaino-sync/usage.md#writer-loop)):
  each block it does not yet hold is folded onto `staged()`, then
  `zaino_sync::apply`d. `handle()` is the `IndexHandle` the NFS reads.
- Fallible only at boot (`StoreError` from `open`). `run` panics on a failed
  commit ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.transparent_address.enabled`. Disabled, its
  methods answer `UNIMPLEMENTED`.
- The index folds every block from genesis, so it cannot start at a later
  height: a spend's output must already be in `receives`.

## Serving

```rust,ignore
// a route's reader: snap.views().transparent_address() (None = disabled)
let reader = reader.as_of(snap.tip().height).with_max_rows(max_address_rows);
let utxos = reader.utxos_of(&addresses, start)?;          // Vec<Vec<AddressUtxo>>, oldest first
let balances = reader.balances(&addresses)?;               // Vec<Zatoshis>
let touched = reader.transactions(&address, start, end)?; // Vec<TransactionRef>
```

- Queries take librustzcash's `TransparentAddress`. Its `script()` is the exact
  locking script the rows were stored under.
- `as_of(tip)` hides rows above `tip`: a receive above it is unseen, and a
  spend above it leaves the output unspent. A snapshot's view can sit above its
  tip during bulk sync.
- `utxos_of` scans from `start` to the tip. `balances` and `transactions` scan
  all of history: a spend in range can consume an output received at any
  earlier height. `transactions` takes both bounds inclusive and asserts
  `start <= end`.
- Multi-address methods answer in input order and batch every spend check into
  one `spent` lookup.
- `with_max_rows(n)` (default `DEFAULT_MAX_ADDRESS_ROWS` = 100,000) bounds the
  `receives` rows one request may walk, across all its addresses. Past it the
  request fails with `ServeError::TooManyRows`. It never returns a short list or
  a partial balance.
- Every method is synchronous and reads mmapped pages, so a transport runs it
  off its async workers (`zaino-grpc`'s scan lane).
- Non-final blocks are `zaino-nfs` layers keyed exactly like the maps. A
  snapshot's reader merges them over the committed store by key.

### What it does not answer

- Transaction bytes: `GetTaddressTransactions` gets its txids here, and
  `zaino-grpc` fetches the raw transactions from a validator.
- Mempool activity: only mined blocks are folded.
- Non-standard scripts: they sit under the opaque key, which no address
  reaches.

## Errors

| Case | Result | gRPC |
|---|---|---|
| more receives than the request may walk | `ServeError::TooManyRows` | `RESOURCE_EXHAUSTED` |
| unspent sum over the money supply (corrupt store) | `ServeError::SupplyExceeded` | `INTERNAL` |
| nothing matched | `Ok`, empty | — |

An empty result is never an error: a gap-limit walk must tell "no transactions"
from "cannot tell". `zaino-grpc` refuses an address encoded for another network
(`INVALID_ARGUMENT`) before it reaches the index.

## Storage

- Layout, merges, filters, checksums and crash recovery belong to the engine.
  A foreign network or format is refused at open.
- Rows encode and decode through named functions in `key.rs`, each pinned by
  golden bytes. A stored row that fails to decode panics, naming the invariant.
- `zainod verify` checks a directory with `DiskEngine::verify(path, &schema)`.
