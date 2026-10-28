# `zaino-source` — usage

`ChainDataSource`: every question Zaino asks one trusted validator, one trait, declared in domain
vocabulary (`zaino-primitives` + its own reply types), each method with its own domain error,
nothing about the transport. Each method, its JSON-RPC call and its answer: the
`ChainDataSource` rustdoc (`chain_data_source.rs`). Blocks come back decoded once, from consensus
bytes, into `zaino_primitives::types::Block`.

Two implementations: `ZcashLegacyRpcClient` (JSON-RPC, zebrad or zcashd), and
`ZebraIndexerGrpcClient`, which fetches blocks over zebrad's indexer gRPC and asks everything
else of its inner legacy client. Tests drive the real legacy client against
[`MockValidator`](#testing-mockvalidator).

What a validator reports about itself (`BlockchainInfo`, `NodeRelease` / `EndOfService`,
`PeerInfo`) is this crate's, beside its parser. Only fields Zaino reads are parsed, so an upgrade
this build does not know is skipped, never a failed poll. `BlockchainInfo::schedule()` refuses a
regtest schedule out of protocol order or without Sapling (`ScheduleError`).

## Building a client: one per validator

```rust,ignore
use zaino_source::{Auth, Endpoint, LinkLimits, Timeouts, ZebraIndexerGrpcClient, ZcashLegacyRpcClient};

let link = LinkLimits { max_connections, response_size_cap };
let endpoint = Endpoint { address, auth: Auth::Cookie(path), timeouts: Timeouts::default(), limits: link };
let legacy = ZcashLegacyRpcClient::new(&endpoint)?;
// or blocks over zebrad's indexer gRPC, the rest through `legacy`:
let grpc = ZebraIndexerGrpcClient::new(legacy, indexer_address, tls.as_ref(), Timeouts::default(), link)?;
// one per validator, handed to `zaino_traffic::TrafficBalancer` as a `TrustedValidator` member
```

- `new` never contacts the validator: an unreachable validator is the caller's retry, never a
  construction failure. `EndpointError` = a bad address or TLS config, an unreadable cookie or
  TLS file.
- `Auth`: `Cookie` sends basic auth as `__cookie__` with the file's token, `Basic` the pair,
  `None` no `Authorization` header.
- One attempt per call. Which validator, how many requests in flight, retries and hedges are the
  balancer's (`zaino-traffic`): this crate never re-sends, and a batch item's refusal (zebrad's
  `-1` work queue full included) is that item's own outcome.
- `LinkLimits`: `max_connections` = idle connections kept to the validator; `response_size_cap`
  (one full block's reply ..= 32 MiB) = the largest JSON-RPC reply buffered, which sets how many
  blocks one `getblock` batch asks.
- Every call is recorded at the transport whoever asked, by `validator` and `method` (a batch's
  `method` = its one method, or `poll` for `get_poll_reading`'s mixed batch); failure labels are
  a fixed set.

## The indexer gRPC

`ZebraIndexerGrpcClient::new` and `ZebraIndexerWatch::new` take `Option<&ZebraIndexerTls>`:
`None` = plaintext, as zebrad serves it; `Some` = mutual TLS, which zakurad requires on any
non-loopback indexer address. `ca_path` verifies the server, `cert_path` + `key_path` are this
client's, `server_name` defaults to the address's host. The PEM files are read in `new`; a
rotated file is read on the next restart.

- A block arrives as its stored consensus bytes in a protobuf `bytes` field: no hex, no JSON.
- `get_blocks_by_height` cuts its heights into consecutive runs, one `GetBlockRange` stream each.
  Stock zebrad lacks the indexer's `GetBlockRange` ([`zaino-proto`](../zaino-proto/README.md#modules)).
- Streams are multiplexed over `ceil(max_connections / 20)` connections (zebrad admits 20 streams
  per connection), with a 4 MiB receive window per stream (a whole block in flight).
- `read` timeout = silence per message, never a total.

The legacy client gets a block as hex inside JSON: the body buffer is sized once from
`Content-Length`, and the hex is decoded by `const-hex` (SIMD) without a UTF-8 pass.

## Error model

```rust,ignore
pub enum QueryError<E> {
    Domain(E),                  // the validator answered; this is the answer
    NonDomain(NonDomainError),  // no answer: unreachable, timed out, unauthorized, undecodable
}
```

`Domain` is never worth asking again. An adapter that reports "no block at that height" as
`NonDomain` makes every caller's retry treat an above-tip probe as an outage: if the validator
replied at all, it is almost certainly `Domain`. `NonDomainError { mode: FailureMode, .. }` keeps
the concrete cause as its `source()`.

Every method answers `Err(QueryError::Unsupported)` when the validator lacks the method
(JSON-RPC `-32601`: zcashd and stock zebrad lack `checkadmission`). It is an answer about the
node, never a failure: a balancer asks the next member and the node stays healthy.

## `ZebraIndexerWatch`: push streams as wake hints

`ZebraIndexerWatch::run(cancel, on_change, on_link)` subscribes to zebrad's `ChainTipChange` and
`MempoolChange` until cancelled: every event calls `on_change()`; `on_link(true)` fires once both
streams are open and `on_link(false)` when either ends after that, then it reconnects on a
500 ms → 30 s ladder. Events are hints only: zebrad ends a lagged stream rather than skip events,
so a consumer that polls on every edge never misses a change. zainod wires the callbacks to
`zaino_traffic::TrafficBalancer::pushed`.

## Batched ports for one poll

The balancer's poll asks one validator `get_poll_reading` (one batch, so the listing and the
answers are tagged with the tip read beside them); the chain view then asks
`get_raw_mempool_transactions` for the bytes of what it newly lists, and header sync
`get_block_links`.

- `PollReading::held`: one answer per height, the hash on the validator's best chain or
  `HeightNotFound` above its tip; an item's own failure leaves the rest of the poll standing. A
  failed metadata read never fails the poll either (`Telemetry::absorb` keeps the last answer).
- The listing's fee is the validator's, sent by zebra as an `f64` and parsed back to exact
  zatoshis. `Inactive` = zebrad's mempool is off until it reaches the network tip.
- `get_block_links` returns raw header bytes, neither decoded nor hashed here
  (`zaino_header_chain::decode_header` recomputes the hash).
- `get_raw_mempool_transactions` batches at most 100 calls and, by `encoded_len`, as many bytes
  as keep the reply within `response_size_cap`. `NotFound` on an item = it left the mempool after
  the listing (a normal race).

## Testing: `MockChain`

The `testing` feature (always on for this crate's own tests; enable it from `[dev-dependencies]`
only) holds the one chain builder every test below the live suite takes its blocks, headers,
fees, mempool and validators from. Every transaction is real consensus bytes written by
librustzcash; every block handed out is `decode_block` of its own bytes and passes Zaino's own
checks.

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
let utxos: Ledger = chain.ledger(tip);               // a tip's unspent outputs, txids, nullifiers
let refused: Result<BlockRef, Refused> = chain.try_mine(|b| b.tx(|t| t.spend(funding)));
let rival: Result<TxId, Refused> = chain.try_submit(|t| t.spend(change)); // nothing held on Err
```

- **A test states** values, scripts, spends, fees and note tags; the chain derives the rest: the
  transaction format and branch from the upgrade active at that height, txids and merkle roots
  from the written bytes, the coinbase script (BIP 34 height + extranonce), unique lock times,
  and deterministic filler for points, proofs, signatures and ciphertexts.
- **Note tags.** Nullifiers and leaves are small `u32`s, canonical in every pool's field; compare
  against `nullifier(tag)` / `commitment(leaf)`.
- **Configure first.** `upgrades`, `varied_work`, `network` and `genesis_with` rebuild genesis,
  so they panic after the first `mine`.
- **Work.** `Work::Limit` (default): every nBits is the regtest limit, so only a longer branch
  wins. A reorg-by-work test declares `varied_work()`; then `outweigh()` gives the branch's next
  block the least work that makes it the best, and `BlockBuilder::bits` sets any nBits.
- **Txids and nullifiers are distinct per branch**, not chain-wide: the same bytes re-mined on a
  competing branch (`raw_tx(chain.tx_bytes(txid))`) is the reorg shape mempool and index tests
  need.
- **Mempool.** `submit` / `submit_raw` admit each on the best after every entry before it,
  through the same `Ledger` mining uses, and panic with the `Refused` rule otherwise
  (`try_submit` returns it, nothing held). Whenever the best moves, the mempool is re-admitted
  on the new best.

Views over a `MockChain` live in the crate that owns each type, behind its `testing` feature:
header chains ([`zaino-header-chain`](../zaino-header-chain/usage.md)), peer sets
([`zaino-traffic`](../zaino-traffic/usage.md)), peers and the UTXO set
([`zaino-chainview`](../zaino-chainview/usage.md#testing-feature-testing)), chain params
([`zaino-nfs`](../zaino-nfs/usage.md)). Finality is the header chain's, never the builder's.

### Invariants

Every block `MockChain` hands out passes these. A construction that would break M2–M4, M6 or
M7 is a `Refused` naming the rule: `try_mine` (chain or `Branch`) returns it with nothing
mined, `mine` panics with it and the height.

| | Invariant |
|---|---|
| M1 | hash = SHA-256d of `encode_header`; genesis pinned byte for byte |
| M2 | `prev_hash` = parent's hash, height = parent + 1, time after the parent's median time past (default time = parent + target spacing) |
| M3 | merkle root = the minted txids'; txids distinct on the branch |
| M4 | exactly one coinbase, in slot 0, spending nothing and stating no fee |
| M5 | `Work::Limit` = every nBits the regtest limit; `outweigh()` = best right after its first block, by the least work that wins |
| M6 | each spend = an output unspent on its own branch (an ancestor, or earlier in the block); nullifiers fresh on the branch; chain value pools never negative (ZIP 209); a stated fee = the derived one |
| M7 | pool data only once its `ShieldedPool::activation_upgrade` is active |
| M8 | every path inserts into `HeaderChain::anchored`, and `MockChain::tip` = `HeaderChain::best` (`zaino-header-chain` proptest) |
| M9 | `MockValidator` serves its best chain only, by height and by hash; each `Lie` fails the body check (`zaino-sync` fetch tests) |

M1–M7: `src/testing/chain.rs` (minting in `mint.rs`, the transaction rules in `ledger.rs`),
tested in `chain/tests.rs`; `encode_header` is pinned against five mainnet headers in the decode
tests.

## Testing: `MockValidator`

One simulated zebrad over a `MockChain`, answering zebrad's JSON-RPC as bytes. A test reaches it
through Zaino's own client, so every test above it runs the production request builder,
envelope, batching, parsers and error classification:

```rust,ignore
use zaino_source::testing::{raw_transaction, raw_transparent, Lie};

let validator = chain.validator(chain.tip());        // one node, following that tip
let info = chain.blockchain_info(tip);               // its getblockchaininfo at `tip`
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
assert_eq!(validator.calls("getblockheader"), 12);   // each batch item counted
```

- **One dispatch table** (`METHODS` in `validator.rs`): each method a plain handler. A name it
  lacks (or `lacks`) answers `-32601`; scripting names are checked against it, so a typo panics.
- **Faults look like the network.** Latency delays a whole request (a batch waits for its slowest
  method); `reachable` / `down` refuse it with HTTP 503; error codes are zebrad's own.
- It answers from its best chain only. Its mempool is entries it `hear`s plus sends it accepts,
  each admitted on its tip by the chain's own `Ledger`; `hear` panics on a txid outside
  `chain.mempool()` or refused on its tip.
- **Lies are byte edits** on what `getblock` serves; each fails the NFS body check by its rule.
- The seam is `RpcClient`'s wire: HTTP in production, an in-process server under `testing` (no
  socket, so a paused clock stays deterministic).

`testing::fixtures`: captured mainnet blocks from `tests/fixtures/` at `HEIGHTS` (`block(height)`),
for tests that need real transactions of every pool and version.

A mock module elsewhere must be gated `#[cfg(any(test, feature = "..."))]`: a bare feature gate
that nothing enables compiles nothing, and its tests silently never run.
