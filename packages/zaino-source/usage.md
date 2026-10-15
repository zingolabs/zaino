# `zaino-source` — usage

`ChainDataSource`: every question Zaino asks one trusted validator, one trait,
declared in domain vocabulary (`zaino-primitives` + its own reply types), each method with its own domain error,
nothing about the transport. Blocks come back decoded once, from consensus bytes,
into `zaino_primitives::types::Block`. Two implementations; a test fake answers
what its test asks and `unimplemented!()`s the rest.

| Method | `ZcashLegacyRpcClient` | `ZebraIndexerGrpcClient` | Answer |
|---|---|---|---|
| `get_block_by_hash(hash)` | `getblock <hash> 0` | indexer `GetBlock` | `Block`, or `GetBlockByHashError::NotFound` |
| `get_blocks_by_height(heights)` | `getblock "<h>" 0`, batched | indexer `GetBlockRange`, one per consecutive run | `Block` per height, or `GetAtHeightError::HeightNotFound` |
| `get_block_links(heights)` | `getblockheader <h> false`, batched | JSON-RPC, as legacy | `BlockLink` per height, or `HeightNotFound` |
| `get_poll_reading(metadata, holds)` | one poll batch (below) | JSON-RPC, as legacy | `PollReading` |
| `get_raw_mempool_transactions(listed)` | `getrawtransaction <txid> 0`, batched | JSON-RPC, as legacy | bytes per entry |
| `get_transaction(txid)` | `getrawtransaction <txid> 1` | JSON-RPC, as legacy | bytes + `TransactionLocation` |
| `send_raw_transaction(bytes)` | `sendrawtransaction` | JSON-RPC, as legacy | the txid, or the rejection |
| `check_admission(bytes)` | `checkadmission` (Zaino's zebra fork; nothing admitted or relayed) | JSON-RPC, as legacy | the txid, `CheckAdmissionError::Refused` (the send's verdict), or `Unsupported` (`-32601`: zcashd, stock zebrad) |

What a validator reports about itself is this crate's, beside its parser:
`BlockchainInfo` (tip, network-tip estimate, each known upgrade's activation;
`schedule()` = those as a `LocalNetwork`, `None` out of protocol order: regtest's
boot reads it), `NodeRelease` / `EndOfService`, `PeerInfo`. Only fields Zaino reads
are parsed, so an upgrade this build does not know is skipped, never a failed poll.

## Building a client: one per validator

```rust,ignore
use zaino_source::{LinkLimits, Timeouts, ZebraIndexerGrpcClient, ZcashLegacyRpcClient};

let link = LinkLimits { max_connections, max_bytes_per_sec };
let legacy = ZcashLegacyRpcClient::at(address, cookie, user, password, Timeouts::default(), link)?;
// or blocks over zebrad's indexer gRPC, the rest through `legacy`:
let grpc = ZebraIndexerGrpcClient::at(legacy, indexer_address, tls.as_ref(), Timeouts::default(), link)?;
// one per validator, handed to `zaino_traffic::TrafficBalancer` as a `Trusted` member
```

`at` builds the client from config without contacting the validator. An
unreachable validator is the caller's retry, never a construction failure;
`EndpointError` means a bad address, an unreadable cookie or TLS file, a user
without a password (or the reverse), or a client that cannot be built. zainod
picks one for every validator (`validator_client`).

Credentials: a cookie path wins (basic auth as `__cookie__`); otherwise `user` +
`password` together are basic auth, and neither sends no `Authorization` header.

## The indexer gRPC: plaintext or mutual TLS

`ZebraIndexerGrpcClient::at` and `ZebraIndexerWatch::at` (the push streams) take
`Option<&ZebraIndexerTls>`. `None` = plaintext, as zebrad serves it. `Some` = mutual TLS, which
zakurad requires on any non-loopback indexer address (its `[rpc.indexer_tls]`):

- `ca_path` verifies the server's certificate; `cert_path` + `key_path` are this client's,
  signed by the server's `client_ca_file`. Deserializes from the same keys (zainod's
  `[trusted_validators.indexer_tls]`).
- `server_name` = the name on the server's certificate (`None` = the address's host).
- The PEM files are read in `at`: an unreadable one is `EndpointError::TlsFile`, naming it. A
  rotated file is read on the next restart.

- `ZcashLegacyRpcClient`: JSON-RPC over HTTP/1.1 (zebrad or zcashd). A block
  arrives as hex inside JSON: the body buffer is sized once from `Content-Length`,
  and the hex string is handed over as raw bytes (no UTF-8 pass) and decoded by
  `const-hex` (SIMD) into an exact-size buffer.
- `ZebraIndexerGrpcClient`: blocks over zebrad's indexer gRPC (HTTP/2), the rest through
  the `ZcashLegacyRpcClient` it holds.
  - A block arrives as its stored consensus bytes in a protobuf `bytes` field, a
    `Bytes` slice of the received frame: no hex, no JSON.
  - `get_blocks_by_height` cuts its heights into consecutive runs, one
    `GetBlockRange` stream each.
  - It needs zebrad's `GetBlockRange` (`elicbarbieri/zebra perf/indexer-block-range`).
  - Streams are multiplexed over `ceil(max_connections / 20)` connections (zebrad
    admits 20 streams per connection), requests rotated across them.
  - Receive windows: 4 MiB per stream (a whole block in flight), every stream's at
    once per connection.
  - `read` timeout = silence per message, never a total.
  - `max_bytes_per_sec` paces each block as received, so HTTP/2 flow control
    slows the sender.
  - Metrics (`GetBlock`, `GetBlockRange`) land in the same families as JSON-RPC.

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
`RpcError(code)`, `GrpcStatus(code)`, `Parse`, `Auth`).

## `ZebraIndexerWatch`: push streams as wake hints

`ZebraIndexerWatch::at(address, timeouts)` names zebrad's indexer gRPC
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
`ChainDataSource`, made by the `ValidatorViews` extension on the chain. N nodes over
one chain = N validators, each following its own tip.

```rust,ignore
use zaino_source::testing::{decoded, raw_transaction, raw_transparent, Lie, Port, ValidatorViews};

let validator = chain.validator(chain.tip());        // a MockValidator
validator.follow(&chain, other_tip);                 // extend, reorg, retreat: any held tip
validator.reorg_after_next_poll(&chain, fork);       // tip read, then `getblockhash` on `fork`
validator.estimate(h(500));                          // getblockchaininfo estimatedheight
let info = chain.blockchain_info(tip);               // getblockchaininfo with `tip` as best
let txid = chain.submit(decoded(raw));               // the network's mempool (MockChain)
validator.hear(&chain, txid);                        // relayed here: listed from the next poll
validator.evict(txid);                               // evicted: unlisted from the next poll
validator.listing(Err(GetMempoolListingError::Inactive));
let fee = SendRawTransactionError::Rejected { code: -26, message: "fee".into() };
validator.relay(Err(fee));                           // sendrawtransaction + checkadmission verdict
validator.checks_admission(false);                   // checkadmission = Unsupported (zcashd)
validator.metadata(None, Some(release));             // peers read times out
validator.latency(&[Port::Block], Duration::from_secs(2)); // before each answer there (tokio::time)
validator.fail_next(2, FailureMode::Timeout);         // any port
validator.reachable(&[Port::Send], false);           // refused in transport there; Port::ALL = gone
validator.lie(Some(Lie::Poisoned));                  // WrongBlock | Poisoned | Mutated | WrongHeight
let served = Lie::Mutated.told(&honest);             // the same shape, no validator (sans-IO models)
validator.tamper(h(80), |header| header.time = early); // a header the builder refuses, rehashed
assert_eq!(validator.calls(), Calls { polls: 3, links: 12, blocks: 0, sends: 1, checks: 0 });
let (txid, raw) = raw_transaction(lock_time, expiry); // a real, empty v4 transaction
let (txid, raw) = raw_transparent(&[OutPoint::new([0x01; 32], 0)], &[(&alice, 4_000)]); // a real spend
chain.mine(|b| b.raw_tx(decoded(raw)));              // mined with its bytes: get_transaction serves them
```

- Answers as zebrad: blocks by hash and links by height from its best chain only,
  nothing above its tip; `getblockchaininfo` = `chain.blockchain_info(tip)` (the
  chain's upgrade schedule, statuses by its tip).
- Its mempool is its own view of the chain's: entries it `hear`s plus sends it accepts,
  each admitted in order on its tip by the chain's own `Ledger` (unspent inputs, fresh
  nullifiers, fee ≥ 0) and listed at the fee its bytes leave there. Nodes differ in what
  they heard, never in the rules. `hear` panics on a txid outside `chain.mempool()` or
  refused on its tip.
- Each `follow` re-admits its mempool on the new tip: mined, conflicting and orphaned
  entries leave. `evict` drops an entry and every entry spending its outputs.
- A send refused on its tip + mempool = `Rejected { code: -25, message }` naming the
  `Refused` rule (e.g. `spends …, not unspent on this branch`); undecodable = `Malformed`.
- `get_transaction` of a mined `TxBuilder` transaction panics: it has no bytes, and
  the double never invents a body (mine real bytes with `raw_tx`).
- Each `Lie` fails the NFS body check (`check_block`) by the rule that names it.
- One double backs the traffic balancer, the chain view, the gRPC routes, the NFS and
  zainod's pipeline alike; reorgs = `follow` another tip of the same `MockChain`.

`testing::fixtures`: captured mainnet blocks from `tests/fixtures/` at `HEIGHTS`
(419,200, 1,000,000, 1,687,104, 2,000,000, 2,500,000). `block(height)` = its consensus
bytes; `transactions(height)` = each of its transactions as its own consensus bytes, for
tests that need real transactions (every pool, every version).

A mock module elsewhere must be gated `#[cfg(any(test, feature = "..."))]`: a
bare feature gate that nothing enables compiles nothing, and its tests silently
never run.
