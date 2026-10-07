# `zaino-source` — usage

`ChainDataSource`: every question Zaino asks one trusted validator's RPC, one
trait, declared in `zaino-primitives` vocabulary, each method with its own domain
error. `ZebraRpcAdapter` answers all of it over zebrad's JSON-RPC; blocks come from
`getblock <id> 0` and are decoded once, from consensus bytes, into
`zaino_primitives::types::Block`. A test fake answers what its test asks and
`unimplemented!()`s the rest.

| Method | RPC | Answer |
|---|---|---|
| `get_block_by_hash(hash)` | `getblock <hash> 0` | `Block`, or `GetBlockByHashError::NotFound` |
| `get_block_links(heights)` | `getblockheader <h> false`, batched | `BlockLink` per height, or `GetBlockError::HeightNotFound` |
| `get_poll_reading(metadata, holds)` | one poll batch (below) | `PollReading` |
| `get_raw_mempool_transactions(listed)` | `getrawtransaction <txid> 0`, batched | bytes per entry |
| `get_transaction(txid)` | `getrawtransaction <txid> 1` | bytes + `TransactionLocation` |
| `send_raw_transaction(bytes)` | `sendrawtransaction` | the txid, or the rejection |

## Building an adapter: one link per validator

```rust,ignore
use zaino_source::{Lane, LinkLimits, Timeouts, ZebraRpcAdapter};

let limits = LinkLimits::new(max_connections, max_requests_per_sec, max_bytes_per_sec)
    .expect("max_connections >= LinkLimits::MIN_CONNECTIONS");
let chainview = ZebraRpcAdapter::at(address, cookie, user, password, Timeouts::default(), limits)?;
let sync = chainview.on(Lane::Sync);
let serve = chainview.on(Lane::Serve);
```

`at` builds the adapter from config without contacting the validator. An
unreachable validator is the caller's retry, never a construction failure;
`EndpointError` means a bad address, an unreadable cookie, or a client that
cannot be built.

Every handle made with `on` shares one link, and so one budget, to that
validator. Zaino never exceeds the budget, however many consumers it has:

| Lane      | Connections        | Used by                                      |
| --------- | ------------------ | -------------------------------------------- |
| `Control` | 2                  | chain view polling, submission (`at` default) |
| `Serve`   | a quarter, at least 1 | wallet lookups (`GetTransaction`, …)      |
| `Sync`    | the rest           | bulk block fetch                             |

- One HTTP/1.1 request holds one connection, so the permits are the connection cap.
  A lane never borrows another's: a sync burst or a wallet storm cannot delay the
  tip poll.
- `max_requests_per_sec` and `max_bytes_per_sec` (`None` = unlimited) are GCRA
  budgets over the whole link. Response bytes are charged per body chunk as they
  are read, so an exhausted budget slows the sender through TCP backpressure.
- Per-validator metrics carry a `validator` label: wait for a permit
  (`zaino.validator_rpc.wait_seconds`, by lane), requests in flight, bytes
  received.

## Error model

```rust,ignore
pub enum QueryError<E> {
    Domain(E),                  // the validator answered; this is the answer
    NonDomain(NonDomainError),  // no answer: unreachable, timed out, unauthorized, undecodable
}
```

`Domain` is never worth asking again. An adapter that reports "no block at
that height" as `NonDomain` makes every caller's retry treat an above-tip probe
as an outage. If the validator replied at all, it is almost certainly `Domain`.

`NonDomainError { mode: FailureMode, .. }` keeps the concrete cause as its
`source()`. `FailureMode::is_transient()` is true for `Connection`, `Timeout`,
`HttpStatus(>= 500)` and RPC codes `-1` (work queue full) and `-28` (warming
up). Every other code is the validator's considered reply. The transport
re-sends a work-queue-full refusal itself, up to `RpcClientConfig::max_retries`.

## `IndexerWatch`: push streams as wake hints

`IndexerWatch::at(address, timeouts)` names zebrad's indexer gRPC
(`indexer_listen_addr`, no auth). `run(cancel, on_change, on_link)` subscribes to
`ChainTipChange` and `MempoolChange` until cancelled: every event calls
`on_change(Change::Tip | Change::Mempool)`; `on_link(true)` fires once both
streams are open and `on_link(false)` when either ends after that (a refused
connect is no edge), then it reconnects on a 500 ms → 30 s ladder. Events are
hints only: zebrad ends a lagged stream rather than skip events, so a consumer
that polls on every edge never misses a change.

## `TrafficBalancer`: which validator answers a read

The set of trusted validators, never one: it is not a `ChainDataSource`. Its one
operation, `failover(|validator| read)`, asks them in turn until one answers:

- first = the cheaper of two at random, cost = peak-EWMA latency × (in flight +
  1) (tower's `PeakEwma` rule); the rest follow cheapest first. The estimate
  jumps to any slower sample, decays toward faster ones, and decays to zero while
  idle, so a once-slow validator is tried again
- a transient failure is retried 3 times on that validator (250 ms, doubling)
- a domain answer (every read's is "absent") moves on: a lagging validator lacks
  a just-mined block or transaction
- the result = the first answer, else a transport failure (that validator may
  have held it), else the last absence

Writes never go through it: submission belongs to the chain view. Block bodies
are fetched by the NFS (`zaino-nfs`), by hash, from any source, each checked
against the verified header.

## Batched ports for one poller

The chain view's poller asks one validator at most two batches per tick (the poll,
then the bytes of what it newly lists); its header sync asks `get_block_links`:

- `get_poll_reading(metadata, holds)`: `getblockchaininfo` +
  `getrawmempool true` + `getblockhash <h>` per height in `holds`, in one batch,
  so the listing and the answers are tagged with the tip read beside them.
  `PollReading::held` has one answer per height, in order: the hash on the
  validator's best chain, `GetBlockError::HeightNotFound` above its tip (zebrad's
  `-32602`, zcashd's `-8`), or that item's own failure (the rest of the poll
  stands). The chain view asks this to learn which validators hold a verified
  block. With `metadata`, also `getpeerinfo` + `getinfo` +
  `getdeprecationinfo`, each its own outcome (`MetadataReading`): telemetry, so
  a failed half never fails the poll. The listing carries each entry's txid,
  fee and `encoded_len`; the fee is the validator's (it resolved the prevouts),
  sent by zebra as an `f64` and parsed back to exact zatoshis. `Unavailable`
  means the validator has no mempool. `Inactive` means zebrad's mempool is off
  until it reaches the network tip (an empty state reports genesis this way).
- `get_block_links(heights)` (`getblockheader <h> false` each):
  `BlockLink { header }` per height, the raw consensus header bytes, neither decoded nor
  hashed here: the consumer decodes once on receipt (`zaino_header_chain::decode_header`
  recomputes the hash from the bytes). `GetBlockError::HeightNotFound` means a height above the tip.
- `get_raw_mempool_transactions(listed)`: the bytes of listed entries, in batches of at most
  100 calls and 8 MiB of transactions by `encoded_len` (the hex reply stays under
  zebrad's response cap). `NotFound` on an item means it left the mempool after
  the listing (a normal race).

In each, items come back in request order and `Err` means the batch as a whole
failed. A crate-internal JSON-RPC batch call is the transport underneath: one
permit, the request budget charged per call, replies matched by `id`.

`NodeRelease` (from `getinfo` + `getdeprecationinfo`) carries the build, user
agent, protocol version and `EndOfService`: `At { height, estimated_unix }` on
mainnet, `NotEnforced` elsewhere, `Unknown` for a zebrad older than 6.3.

## Testing: `MockChain`

Behind the `testing` feature (always compiled for this crate's own tests):

```rust,ignore
use zaino_primitives::testing::Chain;
use zaino_source::{FailureMode, mock::MockChain};

let mut chain = Chain::new();
let tip = chain.extend(chain.genesis().hash, 10);
let mock = MockChain::serving(chain.path(tip.hash))
    .fail_next(2, FailureMode::Timeout); // failure injection

mock.extend_best(chain.path(fork.hash)); // each block becomes the tip: its height and up replaced
mock.rewind_to(height);                  // invalidateblock: heights above leave the best chain
mock.mempool_insert(txid, raw);          // listed from the next poll
mock.set_reachable(false);               // every call fails in transport until set back
```

It serves only real chains: every block it is given must come from
`zaino_primitives::testing::Chain` (its hash = SHA-256d of its encoded header, its parent
the best block below it), and `get_block_links` hands out those header bytes, so a
header chain can verify what it serves. It answers the way zebrad does: blocks by
hash and header links by height come from the best chain only, and nothing is
served above the tip. It is a whole `ChainDataSource`: its tip; its mempool, each entry listed at
`MEMPOOL_FEE`, a sent transaction (txid from its bytes, `Malformed` if they do not
decode) listed from the next poll, a mined txid dropped; `get_transaction` locating a
txid in the mempool or on the best chain; no peers; a release with no halt. One mock
backs the chain view, the gRPC routes and the NFS alike.

`mock::fixture_block(height)` / `mock::fixture_transactions(height)`: a captured
mainnet block from `tests/fixtures/` (419,200, 1,000,000, 1,687,104, 2,000,000,
2,500,000), whole or as each transaction's own consensus bytes, for tests that need
real transactions (every pool, every version).
`zaino-nfs`'s driver test and zainod's pipeline test drive the NFS through reorgs
with `extend_best`.

A mock module elsewhere must be gated `#[cfg(any(test, feature = "..."))]`: a
bare feature gate that nothing enables compiles nothing, and its tests silently
never run.
