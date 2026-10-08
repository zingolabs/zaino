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
| `get_block_links(heights)` | `getblockheader <h> false`, batched | `BlockLink` per height, or `GetAtHeightError::HeightNotFound` |
| `get_poll_reading(metadata, holds)` | one poll batch (below) | `PollReading` |
| `get_raw_mempool_transactions(listed)` | `getrawtransaction <txid> 0`, batched | bytes per entry |
| `get_transaction(txid)` | `getrawtransaction <txid> 1` | bytes + `TransactionLocation` |
| `send_raw_transaction(bytes)` | `sendrawtransaction` | the txid, or the rejection |

## Building an adapter: one link per validator

```rust,ignore
use zaino_source::{LinkLimits, Timeouts, ZebraRpcAdapter};

let link = LinkLimits { max_connections, max_bytes_per_sec };
let adapter = ZebraRpcAdapter::at(address, cookie, user, password, Timeouts::default(), link)?;
// one per validator, handed to `zaino_traffic::TrafficBalancer` as a `Trusted` member
```

`at` builds the adapter from config without contacting the validator. An
unreachable validator is the caller's retry, never a construction failure;
`EndpointError` means a bad address, an unreadable cookie, or a client that
cannot be built.

- One attempt per call. Which validator, how many requests in flight, the request
  rate, retries and hedges are the balancer's (`zaino-traffic`): this crate never
  re-sends, and a batch item's refusal (zebrad's `-1` work queue full included) is
  that item's own outcome.
- `LinkLimits`: `max_connections` = idle connections kept to the validator;
  `max_bytes_per_sec` (`None` = unlimited) paces response bytes per body chunk as
  they are read, so an exhausted budget slows the sender through TCP backpressure.
- Per-validator metrics carry a `validator` label: call duration by method,
  failures, calls in flight, bytes received.

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
`source()`; `mode` classifies it (`Connection`, `Timeout`, `HttpStatus`,
`RpcError(code)`, `Parse`, `Auth`).

## `IndexerWatch`: push streams as wake hints

`IndexerWatch::at(address, timeouts)` names zebrad's indexer gRPC
(`indexer_listen_addr`, no auth). `run(cancel, on_change, on_link)` subscribes to
`ChainTipChange` and `MempoolChange` until cancelled: every event calls
`on_change(Change::Tip | Change::Mempool)`; `on_link(true)` fires once both
streams are open and `on_link(false)` when either ends after that (a refused
connect is no edge), then it reconnects on a 500 ms → 30 s ladder. Events are
hints only: zebrad ends a lagged stream rather than skip events, so a consumer
that polls on every edge never misses a change. zainod wires the callbacks to
`zaino_traffic::TrafficBalancer::pushed`.

## Batched ports for one poll

The balancer's poll asks one validator `get_poll_reading`; the chain view then
asks `get_raw_mempool_transactions` for the bytes of what it newly lists, and its
header sync `get_block_links`:

- `get_poll_reading(metadata, holds)`: `getblockchaininfo` +
  `getrawmempool true` + `getblockhash <h>` per height in `holds`, in one batch,
  so the listing and the answers are tagged with the tip read beside them.
  `PollReading::held` has one answer per height, in order: the hash on the
  validator's best chain, `GetAtHeightError::HeightNotFound` above its tip (zebrad's
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
  recomputes the hash from the bytes). `GetAtHeightError::HeightNotFound` means a height above the tip.
- `get_raw_mempool_transactions(listed)`: the bytes of listed entries, in batches of at most
  100 calls and 8 MiB of transactions by `encoded_len` (the hex reply stays under
  zebrad's response cap). `NotFound` on an item means it left the mempool after
  the listing (a normal race).

In each, items come back in request order and `Err` means the batch as a whole
failed. A crate-internal JSON-RPC batch call is the transport underneath: one
HTTP request, replies matched by `id` (the balancer charges its request rate per
call).

`NodeRelease` (from `getinfo` + `getdeprecationinfo`) carries the build, user
agent, protocol version and `EndOfService`: `At { height, estimated_unix }` on
mainnet, `NotEnforced` elsewhere, `Unknown` for a zebrad older than 6.3.

## Testing: `MockValidator`

Behind the `testing` feature (always compiled for this crate's own tests): one
simulated zebrad over a `zaino_primitives::testing::MockChain`, a whole
`ChainDataSource`. N nodes over one chain = N validators, each following its own tip.

```rust,ignore
use zaino_source::testing::{decoded, raw_transaction, raw_transparent, Lie, MockValidator, Port};

let validator = MockValidator::following(&chain, chain.tip());
validator.follow(&chain, other_tip);                 // extend, reorg, retreat: any held tip
validator.reorg_after_next_poll(&chain, fork);       // tip read, then `getblockhash` on `fork`
validator.estimate(h(500));                          // getblockchaininfo estimatedheight
let txid = validator.mempool_insert(raw, 2_000);     // listed from the next poll at 2 000
validator.mempool_remove(txid);                      // evicted: unlisted from the next poll
validator.listing(Err(GetMempoolListingError::Inactive));
let fee = SendRawTransactionError::Rejected { code: -26, message: "fee".into() };
validator.relay(Err(fee));                           // sendrawtransaction verdict
validator.metadata(None, Some(release));             // peers read times out
validator.latency(&[Port::Block], Duration::from_secs(2)); // before each answer there (tokio::time)
validator.fail_next(2, FailureMode::Timeout);         // any port
validator.reachable(&[Port::Send], false);           // refused in transport there; Port::ALL = gone
validator.lie(Some(Lie::Poisoned));                  // WrongBlock | Poisoned | Mutated | WrongHeight
let served = Lie::Mutated.told(&honest);             // the same shape, no validator (sans-IO models)
validator.tamper(h(80), |header| header.time = early); // a header the builder refuses, rehashed
assert_eq!(validator.calls(), Calls { polls: 3, links: 12, blocks: 0, sends: 1 });
let (txid, raw) = raw_transaction(lock_time, expiry); // a real, empty v4 transaction
let (txid, raw) = raw_transparent(&[outpoint([0x01; 32], 0)], &[(&alice, 4_000)]); // a real spend
chain.mine(|b| b.raw_tx(decoded(raw)));              // mined with its bytes: get_transaction serves them
```

- Answers as zebrad: blocks by hash and links by height from its best chain only,
  nothing above its tip; `getblockchaininfo` = `chain.blockchain_info(tip)` (the
  chain's upgrade schedule, statuses by its tip); a mined txid leaves the mempool.
- An accepted send is listed at the fee its bytes leave over its best chain; an input
  not unspent there = `Rejected { code: -25, message: "missing input …" }`; undecodable =
  `Malformed`.
- `get_transaction` of a mined `TxBuilder` transaction panics: it has no bytes, and
  the double never invents a body (mine real bytes with `raw_tx`).
- Each `Lie` fails the NFS body check (`check_block`) by the rule that names it.
- One double backs the traffic balancer, the chain view, the gRPC routes, the NFS and
  zainod's pipeline alike; reorgs = `follow` another tip of the same `MockChain`.

`testing::fixtures::transactions(height)`: each transaction of a captured mainnet block
from `tests/fixtures/` (419,200, 1,000,000, 1,687,104, 2,000,000, 2,500,000) as its own
consensus bytes, for tests that need real transactions (every pool, every version).

A mock module elsewhere must be gated `#[cfg(any(test, feature = "..."))]`: a
bare feature gate that nothing enables compiles nothing, and its tests silently
never run.
