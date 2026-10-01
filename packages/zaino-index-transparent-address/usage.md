# zaino-index-transparent-address

The transparent-address index. Backs `GetAddressUtxos[Stream]`,
`GetTaddressBalance[Stream]` and `GetTaddressTransactions` (plus its deprecated
alias `GetTaddressTxids`) from two
[`zaino_persistence::lsm`](../zaino-persistence/usage.md#lsm-segments) sets, each a
pure projection of a block:

```text
receives   addr(21) ‖ height u32 ‖ txid(32) ‖ vout u32   ->  value_zat u64
spent      txid(32) ‖ vout u32                           ->  height u32, spending txid
```

Keys are big-endian, so byte order is key order.

## Wiring

```rust
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressService};
use zcash_transparent::address::TransparentAddress;

let index = TransparentAddressIndexWriter::open(fs, &path, network, batch_bytes)?;
let service = TransparentAddressService::new(index.published().served(), network);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
let subscription = block_sink.subscribe(TransparentAddressIndexWriter::NAME, queue_bytes);
tokio::spawn(index.run(subscription)); // infallible: returns at Shutdown

let address = TransparentAddress::PublicKeyHash(hash160);
let unspent = service.utxos(&address, start)?;              // start (inclusive) to the tip; oldest first
let balance = service.balance(&address)?;                   // Zatoshis
let touched = service.transactions(&address, start, end)?; // both inclusive; start <= end, asserted

// many addresses, one view and one batched spend lookup (answers in `addresses` order)
let per_address = service.utxos_of(&addresses, start)?;     // Vec<Vec<AddressUtxo>>
let balances = service.balances(&addresses)?;            // Vec<Zatoshis>
```

- Every method is synchronous and reads mmapped segments: a transport runs it
  off its async workers (`zaino-grpc`'s scan lane).
- `with_max_rows(n)` (default `DEFAULT_MAX_ADDRESS_ROWS` = 100,000) bounds the
  receives one request walks, across all its addresses and both tiers. The walk
  stops at `n + 1` (`Snapshot::range_at_most`), and the request is
  `ServeError::TooManyRows { limit }`, never a short list or a partial balance.
  Every method walks the address's whole history, whatever range it asks about.
- A request's spend checks are batched: every received outpoint of every
  address in it resolves through one sorted `get_many` over the `spent`
  segments, not one probe per outpoint.

- `TransparentAddressIndexWriter::run` is this index's own loop over its
  `zaino_sync::BlockSink` subscription (one `match` per `Step`), keeping its
  own final blocks until they commit. Services read what it publishes
  (`published().served()`), gated by `published().gate(..)`'s task.
- Fallible only at boot (`open` → `StoreError`). `run` is infallible: it panics
  on a failed commit ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
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
sets: scan `receives` for the address, probe `spent` per outpoint; unspent = the
probes that miss. Cost is `O(received)` per address. See
[`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §5.

`transactions(start, end)` (both inclusive) scans all of history (an in-range spend consumes an
output received at any height) and reports both receipts and spends in range.

## Applied vs finalised

Both are tips: the last height held, inclusive (`None` = nothing held).

| | covers | moved by |
|---|---|---|
| applied | non-finalized tier, reaches the tip | a non-final `Apply`, a commit, a `Reorg` |
| `durable_tip()` | durable segments | a commit |

- A non-final block folds into in-memory `imbl` maps (`NonFinalizedRows`)
  inline; no I/O.
- Final blocks are staged: in bulk until a batch's bytes fill, at the tip on
  every `Finalized`. One commit per batch drains the non-finalized rows, projects
  every block they never saw, and writes one segment per set on the blocking
  pool; the loop awaits it before the next step.
- `Reorg` writes what is final, then drops all non-finalized rows; no segment
  is touched. A reorg and a restart are the same operation: re-apply from the
  durable tip. See
  [`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).
- Reads pin one `ReadView`: the non-finalized rows and both segment sets, taken
  together by the loop (a row sits in exactly one tier), answering up to the
  applied height.
- Applying asserts contiguous heights from genesis: a gap would leave spent
  outputs counted as balance, so there is no start-height option.

## Manifest and merges

```text
<dir>/
  MANIFEST              count, tip hash, listed segments per set: SegmentMeta { id, records, sealed }
  receives/<id>.seg     + <id>.seg.crc
  spent/<id>.seg        + <id>.seg.crc   (spent segments carry a BinaryFuse8 filter)
```

- A write seals one segment per set, fsyncs both directories, then
  commits `MANIFEST` listing them (the commit point;
  [`docs/design/durability.md`](../../docs/design/durability.md)). Only then
  are the segments published to readers and the height advanced.
- `open(fs, path, network, batch_bytes)` removes every segment the manifest does not list
  (an uncommitted batch or merge output) and opens every listed one at its seal
  (length + tail page). A missing or short segment is fatal; so is a foreign
  network or format.
- After each commit, a set listing 8 (`FANOUT`) idle segments in one size tier
  merges them on a background thread (`zaino_persistence::lsm::SegmentLog`). The
  output lands in the next commit's manifest in place of its inputs, and the
  inputs are unlinked only after it. So the listed segments never share a row, and
  reads need no dedupe beyond non-finalized vs segments. A commit waits on a merge
  only when that tier falls behind (bounded segment count).
- A spend probe asks each `spent` segment's filter first; a miss costs no read.
- `committed_files(dir, network)` = every listed segment and its seal, for
  `zainod verify`.

## Errors

| Case | Result | gRPC |
|---|---|---|
| not yet synced to the tip | `ServeError::Syncing` | `UNAVAILABLE` |
| unspent sum over the money supply (corrupt segments) | `ServeError::SupplyExceeded` | `INTERNAL` |
| synced, nothing matched | `Ok`, empty | — |

`Syncing` is checked before any other validation and carries no progress. An
empty result is never an error: a gap-limit walk must tell "no transactions"
from "cannot tell".

`service.network()` = the index's network; `zaino-grpc` refuses an address
encoded for another network (`INVALID_ARGUMENT`) before it reaches the index.
