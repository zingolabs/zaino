# `zaino-source` — usage

`ChainDataSource`: every question Zaino asks one trusted validator, one trait,
declared in domain vocabulary (`zaino-primitives` + its own reply types), each method with its
own domain error, nothing about the transport. Blocks come back decoded once, from consensus bytes,
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
| `check_admission(bytes)` | `checkadmission` (Zaino's zebra fork; nothing admitted or relayed) | JSON-RPC, as legacy | the txid, or the send's verdict (`SendRawTransactionError`) |
| `get_standard_fee()` | `getstandardfee` | JSON-RPC, as legacy | `StandardFee` (ZIP 317 marginal fee + estimator version) |

Every method answers `Err(QueryError::Unsupported)` when the validator has no such method
(JSON-RPC `-32601`: zcashd and stock zebrad lack `checkadmission`, older ones
`getstandardfee`). It is classified once, for every call, and is an answer about the node:
never a failure, so a balancer asks the next member and the node stays healthy.

What a validator reports about itself is this crate's, beside its parser:
`BlockchainInfo` (tip, network-tip estimate, each known upgrade's activation;
`schedule()` = those as a `LocalNetwork`, or `ScheduleError::OutOfOrder` /
`NoSapling`: regtest's boot reads it and refuses either), `NodeRelease` / `EndOfService`, `PeerInfo`. Only fields Zaino reads
are parsed, so an upgrade this build does not know is skipped, never a failed poll.

## Building a client: one per validator

```rust,ignore
use zaino_source::{Auth, Endpoint, LinkLimits, Timeouts, ZebraIndexerGrpcClient, ZcashLegacyRpcClient};

let link = LinkLimits { max_connections, max_bytes_per_sec, response_size_cap };
let endpoint = Endpoint { address, auth: Auth::Cookie(path), timeouts: Timeouts::default(), limits: link };
let legacy = ZcashLegacyRpcClient::new(&endpoint)?;
// or blocks over zebrad's indexer gRPC, the rest through `legacy`:
let grpc = ZebraIndexerGrpcClient::new(legacy, indexer_address, tls.as_ref(), Timeouts::default(), link)?;
// one per validator, handed to `zaino_traffic::TrafficBalancer` as a `TrustedValidator` member
```

`new` builds the client without contacting the validator. An unreachable
validator is the caller's retry, never a construction failure; `EndpointError`
means a bad address, an unreadable cookie or TLS file, or a client that cannot
be built. zainod picks one for every validator (`validator_client`).

`Auth`: `Cookie` sends basic auth as `__cookie__` with the file's token, `Basic`
sends the pair, `None` sends no `Authorization` header.

## The indexer gRPC: plaintext or mutual TLS

`ZebraIndexerGrpcClient::new` and `ZebraIndexerWatch::new` (the push streams) take
`Option<&ZebraIndexerTls>`. `None` = plaintext, as zebrad serves it. `Some` = mutual TLS, which
zakurad requires on any non-loopback indexer address (its `[rpc.indexer_tls]`):

- `ca_path` verifies the server's certificate; `cert_path` + `key_path` are this client's,
  signed by the server's client CA. Deserializes from the same keys (zainod's
  `[trusted_validators.indexer_tls]`).
- `server_name` = the name on the server's certificate (`None` = the address's host).
- The PEM files are read in `new`: an unreadable one is `EndpointError::TlsFile`, naming it. A
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
  - It needs a zebrad serving the indexer's `GetBlockRange`, which stock zebrad lacks
    ([`zaino-proto`](../zaino-proto/README.md#modules)).
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
  they are read, so an exhausted budget slows the sender through TCP backpressure;
  `response_size_cap` (`ResponseSizeCap`: one full block's reply ..= 32 MiB) = the largest
  JSON-RPC reply buffered, and sets how many blocks one `getblock` batch asks.
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

`ZebraIndexerWatch::new(address, tls, timeouts)` names zebrad's indexer gRPC
(`indexer_listen_addr`; `tls` as above). `run(cancel, on_change, on_link)` subscribes to
`ChainTipChange` and `MempoolChange` until cancelled: every event on either calls
`on_change()`; `on_link(true)` fires once both
streams are open and `on_link(false)` when either ends after that (a refused
connect is no edge), then it reconnects on a 500 ms → 30 s ladder. Events are
hints only: zebrad ends a lagged stream rather than skip events, so a consumer
that polls on every edge never misses a change. zainod wires the callbacks to
`zaino_traffic::TrafficBalancer::pushed`.

## Batched ports for one poll

The balancer's poll asks one validator `get_poll_reading`; the chain view then
asks `get_raw_mempool_transactions` for the bytes of what it newly lists, and its
header sync `get_block_links`:

- `get_poll_reading(metadata, holds)`: one batch (its calls: the method's rustdoc), so the
  listing and the answers are tagged with the tip read beside them.
  `PollReading::held` has one answer per height, in order: the hash on the
  validator's best chain, `GetAtHeightError::HeightNotFound` above its tip (zebrad's
  `-32602`, zcashd's `-8`), or that item's own failure (the rest of the poll
  stands). The chain view asks this to learn which validators hold a verified
  block. With `metadata`, `MetadataReading` = the answered `Telemetry` + each failed read,
  named: a failed read never fails the poll, and `Telemetry::absorb` keeps the last answer
  in its place. The listing carries each entry's txid,
  fee and `encoded_len`; the fee is the validator's (it resolved the prevouts),
  sent by zebra as an `f64` and parsed back to exact zatoshis. `Unavailable`
  means the validator has no mempool. `Inactive` means zebrad's mempool is off
  until it reaches the network tip (an empty state reports genesis this way).
- `get_block_links(heights)` (`getblockheader <h> false` each):
  `BlockLink { header }` per height, the raw consensus header bytes, neither decoded nor
  hashed here: the consumer decodes once on receipt (`zaino_header_chain::decode_header`
  recomputes the hash from the bytes). `GetAtHeightError::HeightNotFound` means a height above
  the tip.
- `get_raw_mempool_transactions(listed)`: the bytes of listed entries, in batches of at most
  100 calls and, by `encoded_len`, as many bytes as keep the hex reply within
  `response_size_cap` (an entry over that budget goes alone). `NotFound` on an item means it
  left the mempool after the listing (a normal race).

In each, items come back in request order and `Err` means the batch as a whole
failed. A crate-internal JSON-RPC batch call is the transport underneath: one
HTTP request, replies matched by `id` (the balancer charges its request rate per
call).

`NodeRelease` (from `getinfo` + `getdeprecationinfo`) carries the build, user
agent and `EndOfService`: `At { height }` on
mainnet, `NotEnforced` elsewhere, `Unknown` for a zebrad older than 6.3.

## Testing: `MockChain`

The `testing` feature (always on for this crate's own tests; enable it from
`[dev-dependencies]` only) holds the one chain builder every test below the live suite takes
its blocks, headers, fees, mempool and validators from. Every transaction is real consensus
bytes, written by librustzcash; every block handed out is `decode_block` of its own bytes.

```rust,ignore
use zaino_source::testing::{all_at, commitment, h, nullifier, p2pkh, Ledger, MockChain, Unmined};
use zcash_protocol::ShieldedPool;

let alice = p2pkh([0xaa; 20]);
let mut chain = MockChain::regtest();                // every upgrade through NU6.3 at 1, bare genesis
let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
let funding = chain.output(funded, 0, 0);            // block, tx slot, vout → OutPoint
let spent = chain.mine(|b| {
    b.tx(|t| t.spend(funding).pay(&alice, 30_000).pay(&alice, 19_000).fee(1_000))
        .tx(|t| {
            t.spend_in_block(1, 1)                   // slot 1's vout 1, minted just before
                .sapling_output(7)                   // cmu = commitment(7)
                .orchard_action(4, 9)                // nf = nullifier(4), cmx = commitment(9)
                .value_balance(ShieldedPool::Sapling, -19_000)
        })
});
let txid = chain.tx(spent, 1).txid;                  // the hash of its bytes
let raw = chain.tx_bytes(txid);                      // every transaction keeps its bytes
let served = chain.block_bytes(spent.hash);          // what `getblock <hash> 0` serves
let fees = chain.fees(spent.hash);                   // [Coinbase, Paid(1 000), Paid(0)]

let tip = chain.mine_empty(3);                       // bare blocks on the best tip
let blocks = chain.blocks(tip);                      // genesis ..= tip, as `Arc<Block>`s
let side = chain.fork(h(2)).mine_empty(1).tip();     // a side branch: not best
let mut varied = MockChain::regtest().varied_work(); // reorg-by-work tests opt in
let twelve = varied.mine_empty(12);
let retreat = varied.fork(h(10)).outweigh().mine_empty(1).tip(); // 12 → 11, now the best

let change = chain.output(spent, 1, 0);
let parent = chain.submit(|t| t.spend(change).pay(&alice, 29_000)); // minted for the next block
let txid = chain.submit_raw(raw_transaction(1, 0).1); // bytes from elsewhere
let held: &[Unmined] = chain.mempool();              // submission order, each with bytes + fee
let mined = chain.mine_mempool();                    // one block on tip() carrying all of it
let utxos: Ledger = chain.ledger(tip);               // a tip's unspent outputs, txids, nullifiers
```

- **Who writes what.** A test states values, scripts, spends, fees and note tags. The
  chain derives everything else:
  - the transaction format and branch from the upgrade active at that height (v1 before
    Overwinter, v3, v4 from Sapling, v5 from NU5, v6 when Ironwood actions are present, v4 when
    Sprout is)
  - txid = the hash of the written bytes; merkle root over them
  - the coinbase script = BIP 34 height + extranonce; an NU5+ coinbase expires at its height
    (ZIP 203: a v5 txid skips `scriptSig`, so this is its height commitment)
  - each other transaction's lock time = a mint counter (unique bytes, always final)
  - points, proofs, signatures, ciphertexts = deterministic filler (librustzcash's lazy
    readers never decompress them)
- **Note tags.** Nullifiers and leaves are small `u32`s, little-endian, canonical in every
  pool's field; compare against `nullifier(tag)` / `commitment(leaf)`.
- **A balance needs a description.** `value_balance(pool, ..)` without a spend, output or
  action in that pool panics (an empty bundle carries no balance); Sprout + Orchard / Ironwood
  in one transaction panics (no format carries both).
- **Configure first.** `upgrades`, `varied_work`, `network` and `genesis_with` rebuild
  genesis, so they panic after the first `mine`.
- **Work.** `Work::Limit` (default): every nBits is the regtest limit, so only a longer
  branch wins. A reorg-by-work test declares `varied_work()`; then `outweigh()` gives the
  branch's next block the least work that makes it the best, and `BlockBuilder::bits`
  sets any nBits.
- **Txids and nullifiers are distinct per branch**, not chain-wide: the same bytes re-mined
  on a competing branch (`raw_tx(chain.tx_bytes(txid))`) is the reorg shape mempool and index
  tests need.
- **Mempool.** `submit` / `submit_raw` admit each on the best after every entry before it,
  through the same `Ledger` mining uses, and panic with the `Refused` rule otherwise. Whenever
  the best moves, the mempool is re-admitted on the new best: transactions only the old best
  mined come back first (block order, coinbases never), and mined, conflicting or orphaned
  entries leave.

### Views in other crates

Each lives in the crate that owns its type, behind that crate's `testing` feature:

| View | Crate | Gives |
|---|---|---|
| `HeaderViews` (trait on `MockChain`) | `zaino-header-chain` | `header_params()`, `header_chain(depth)`, `verified(tip)`, `verified_finalized(tip, finalized_at)` |
| `ValidatorViews` (trait on `MockChain`) | here | `blockchain_info(tip)`; `validator(tip)`: a `MockValidator` (below) |
| `MockPeerSet::new(&chain, tip)` | `zaino-traffic` | a `PeerTransport` over one path, `hold(height)` to move its tip |
| `MockPeers` | `zaino-chainview` | a `PeerSource`: live peers list mempools and take pushes, dead ones refuse |
| `UnspentOutputs for Ledger` | `zaino-chainview` | `chain.ledger(tip)` as the UTXO set a peer quorum's prevouts are priced from |
| `ChainParams::of(&chain)` | `zaino-nfs` | the chain's network label and upgrade schedule |

Finality is the header chain's, never the builder's: ask for it with `verified_finalized`.

### Invariants

Every block `MockChain` hands out passes these. A construction that would break one
panics with a message naming the rule. The labels appear in test docs across the
workspace.

| | Invariant |
|---|---|
| M1 | hash = SHA-256d of `encode_header`; genesis pinned byte for byte |
| M2 | `prev_hash` = parent's hash, height = parent + 1, time after the parent's median time past (default time = parent + target spacing: 150 s pre-Blossom, 75 s, 25 s from NU7) |
| M3 | merkle root = the minted txids'; txids distinct on the branch |
| M4 | exactly one coinbase, in slot 0, spending nothing and stating no fee |
| M5 | `Work::Limit` = every nBits the regtest limit; `outweigh()` = best right after its first block, by the least work that wins |
| M6 | each spend = an output unspent on its own branch (an ancestor, or earlier in the block); nullifiers fresh on the branch; chain value pools never negative (ZIP 209); a stated fee = the derived one |
| M7 | pool data only once its upgrade is active (Sapling; Orchard from NU5; Ironwood from NU6.3) |
| M8 | every path inserts into `HeaderViews::header_chain`, and `MockChain::tip` = `HeaderChain::best` (`zaino-header-chain` proptest) |
| M9 | `MockValidator` serves its best chain only, by height and by hash; each `Lie` fails the body check (`zaino-sync` fetch tests) |

M1–M7: `src/testing/chain.rs` (minting in `mint.rs`, the transaction rules in `ledger.rs`),
tested in `chain/tests.rs`. `encode_header` / `header_hash` = the consensus header bytes and
their SHA-256d (version 4, a zero 36-byte regtest solution, a mint counter as each nonce),
pinned against five mainnet headers in the decode tests.

## Testing: `MockValidator`

One simulated zebrad over a `MockChain`, answering zebrad's JSON-RPC as bytes. A test reaches
it through Zaino's own client, so every test above it runs the production request builder,
envelope, batching, parsers and error classification:

```rust,ignore
use zaino_source::testing::{raw_transaction, raw_transparent, Lie, ValidatorViews};

let validator = chain.validator(chain.tip());        // one node, following that tip
let source = Arc::new(validator.client());           // a ZcashLegacyRpcClient, served in-process
validator.follow(&chain, other_tip);                 // extend, reorg, retreat: any held tip
validator.reorg_after_next_poll(&chain, fork);       // tip read, then `getblockhash` on `fork`
validator.estimate(h(500));                          // getblockchaininfo estimatedheight
validator.hear(&chain, txid);                        // relayed here: listed from the next poll
validator.evict(txid);                               // evicted: unlisted from the next poll
validator.listing(Err(GetMempoolListingError::Inactive));
validator.relay(Err(fee_too_low));                   // sendrawtransaction + checkadmission verdict
validator.lacks(&["checkadmission"]);                // an older node: -32601 (zcashd, stock zebrad)
validator.telemetry(Telemetry { peers: None, release: Some(release) }); // getpeerinfo: -1
validator.latency(&["getblock"], Duration::from_secs(2)); // before each request carrying it
validator.reachable(&["sendrawtransaction"], false); // requests carrying it: HTTP 503
validator.down(true);                                // every request: HTTP 503
validator.lie(Some(Lie::Poisoned));                  // WrongBlock | Poisoned | Mutated | WrongHeight
validator.tamper(h(80), |header| header.time = early); // a header the builder refuses, rehashed
assert_eq!(validator.calls("getblockheader"), 12);   // each batch item counted
```

- **One dispatch table** (`METHODS` in `validator.rs`): each method a plain handler, params in,
  zebrad's result or `(code, message)` out. A name it lacks (or `lacks`) answers `-32601`.
  Scripting names are checked against it: a typo panics.
- **Faults look like the network.** Latency delays a whole request (a batch waits for its
  slowest method); `reachable` / `down` refuse it with HTTP 503 before anything is answered;
  `-1` / `-5` / `-8` / `-25` / `-32602` are zebrad's own codes.
- Answers from its best chain only, nothing above its tip. Its mempool is its own view of the
  chain's: entries it `hear`s plus sends it accepts, each admitted in order on its tip by the
  chain's own `Ledger` and listed at the fee its bytes leave there. `hear` panics on a txid
  outside `chain.mempool()` or refused on its tip; `follow` re-admits on the new tip.
- **Lies are byte edits** on what `getblock` serves: a flipped nonce, an appended or repeated
  transaction, a coinbase claiming the next height (a v5 coinbase's txid skips `scriptSig`, so
  `WrongHeight` keeps a valid merkle root). Each fails the NFS body check by its rule.
- The seam is `RpcClient`'s wire: HTTP in production, an in-process server under `testing` (no
  socket, so a paused clock stays deterministic). Status, body cap and pacing over real HTTP
  are `rpc/client.rs`'s own tests.

`testing::fixtures`: captured mainnet blocks from `tests/fixtures/` at `HEIGHTS`
(419,200, 1,000,000, 1,687,104, 2,000,000, 2,500,000). `block(height)` = its consensus
bytes; `transactions(height)` = each of its transactions as its own consensus bytes, for
tests that need real transactions (every pool, every version).

A mock module elsewhere must be gated `#[cfg(any(test, feature = "..."))]`: a
bare feature gate that nothing enables compiles nothing, and its tests silently
never run.
