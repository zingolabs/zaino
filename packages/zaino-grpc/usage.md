# zaino-grpc

The wallet-facing `CompactTxStreamer` endpoint: one [`GrpcService`], every method
dispatched by path over [`Routes`]. Every method that reads chain, index, mempool or
validator state loads one `zaino_snapshot::GrpcSnapshot` and reads it for the life of its
request or stream. The per-method table and the status codes a client must distinguish
live in [`docs/rpc_api.md`](../../docs/rpc_api.md).

## Routes

```rust
let routes = Routes {
    snapshots: publisher.handle(),       // zaino_snapshot::Snapshots<V>: one load per request
    submit: Arc::clone(&view),           // Arc<ChainView<S>>: SendTransaction
    validators,                          // zaino_traffic::TrafficBalancer<S>: GetTransaction
    network,                             // declared (GetLightdInfo.chainName)
    version: env!("CARGO_PKG_VERSION"),  // the daemon's (GetLightdInfo.version)
    max_address_rows,                    // receives one t-address request may walk
    indexes,                             // Vec<IndexKind>: the configured ones
};
let bound = GrpcService::new(grpc_listen_address, limits)
    .with_routes(routes)
    .with_trusted_proxies(proxies)
    .with_tls(tls)
    .bind()
    .await?;
tokio::spawn(bound.run(cancel.child_token()));
```

A server can bind and run before its routes exist (an owner still booting or downloading a
snapshot): `GrpcService::new` alone answers health from the start and every method
`UNAVAILABLE` (`starting: not routed yet`). Take its `routing()` handle before `bind()` and
call `route(routes)` once they exist. Routing twice panics.

```rust
let server = GrpcService::new(grpc_listen_address, limits).with_health(health);
let routing = server.routing();
tokio::spawn(server.bind().await?.run(cancel.child_token()));
// … boot …
routing.route(routes);
```

| Method | Answered by (one snapshot) |
|---|---|
| `GetLatestBlock` | `served()?` tip itself (height + hash, no read) |
| `GetBlock`, `GetBlockRange[Nullifiers]` | `served()?`'s `compact_block` (stored records, never re-encoded) |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | `indexed()`'s served `tree_state` (memos keyed on that publish) |
| `GetAddressUtxos[Stream]`, `GetTaddressBalance[Stream]` | `served()?`'s `transparent_address` |
| `GetTaddressTransactions` (+ deprecated `GetTaddressTxids`) | `transparent_address` names them, `validators` supply the bytes |
| `GetMempoolTx` | `mempool()?` (the servable mempool) |
| `GetMempoolStream` | `mempool_stream()?`: the epoch at the served tip, ended when it moves |
| `GetLightdInfo` | `lightd()?`: the served height, the verified best as `estimatedHeight`, the branch and next upgrade from the upgrade schedule at the served height; `network` as configured |
| `SendTransaction` | `submit` (chain view submission; no snapshot) |
| `GetTransaction` | `validators.transaction(txid)` (absent → the next; `NOT_FOUND` only when every one asked said absent) |

- **One snapshot per request** (`snapshots.load()`, pinned for the request or
  the whole stream, [G1](../zaino-snapshot/usage.md#invariants-and-tests)). No height named = the served tip, which every enabled
  index holds: `GetLatestBlock`, `GetLatestTreeState`,
  `GetLightdInfo.blockHeight`, subtree roots completing at or below it, the
  transparent reader `.as_of(tip)`. A height named (`GetBlock`, `GetBlockRange`,
  `GetTreeState`, by height or hash) = up to `At::answers_through(kind)`, the
  index's own durable tip when past the served one. A point read past it =
  `NOT_FOUND`; a range streams to it, then `OUT_OF_RANGE`.
- Wire contract: `SendTransaction`
  rejections = gRPC `OK` + the validator's code and message; `GetTreeState` = the
  exact height, `""` below a pool's activation, `000000` for an empty active
  tree; `GetSubtreeRoots` whole or refused before the first byte (unknown pool
  `UNIMPLEMENTED`, `startIndex` past the end = empty `OK`). See
  [`docs/rpc_api.md`](../../docs/rpc_api.md).
- `Unavailable` = `UNAVAILABLE` with its message: nothing served yet (a booting
  NFS), no verified chain, or no trusted validator holding the best block (the
  mempool methods).
- `GetMempoolStream` ends once the **served** tip moves (after the NFS folds the
  new block), so the `GetLatestBlock` a wallet sends after the end already
  answers the new tip.
- An index missing from `Routes::indexes` = disabled: its methods are
  `UNIMPLEMENTED` from routing on, served or not, naming the index
  (`GetTreeState needs the tree_state index, which is not enabled`). A snapshot
  lacking a configured index's view answers the same. Compact-block is optional
  like the rest. An unknown path is `UNIMPLEMENTED`.
- `S` = the validators' `ChainDataSource`. Derived answers have no validator
  method to fall back to: they come from an index or not at all — see
  [architecture §1](../../docs/architecture.md#1-who-owns-what).
- `V` = the indexes' `zaino_persistence::DurableView` (zainod: `DiskView`): every read = an
  index's reader over a committed snapshot + the NFS's layers, on a read lane (any thread).
  Any engine's committed view serves end to end: `Routes<S, V>`, `GrpcService<S, V>`.
- `GetBlock` and `GetTreeState` by `BlockID.hash` resolve through the
  snapshot's `block_hash` view: `height_of(hash)` names the height (above the
  tip = `NOT_FOUND`), and the answering index must hold the same hash there
  (else `NOT_FOUND`). With block-hash disabled, a hash request is
  `UNIMPLEMENTED`.
- Route tests build a snapshot with `zaino_snapshot::Snapshots::fixed` over
  `Indexed::fixed` + `ChainViewSnapshot::fixed` (features `testing`), or a live
  `Publisher` over a test-fed NFS watch.
- `GetLightdInfo` never calls a validator: one snapshot load (`lightd()`), so it
  answers while no trusted validator vouches (degraded) too; nothing served =
  `UNAVAILABLE`.
- `GetMempoolTx` parses each mempool transaction once (`decode_transaction` +
  `compact_tx`, cached on the entry's `Projection`); each request only renumbers
  slots and prunes pools.
- A t-address encoded for a network other than the transparent index's is
  `INVALID_ARGUMENT`.
- Two network spellings: `chainName` and the tree state's `network` =
  lightwalletd's `main` / `test` / `regtest`; `network_name` (exported) =
  zainod's `mainnet` / `testnet` / `regtest` (config, logs, the t-address
  mismatch message). No surface writes its own match.
- `limits: GrpcLimits` (see [Serve stack](#serve-stack)); zainod fills it from
  its `[grpc]` config section.
- `bind().await` takes the socket and returns a `BoundGrpcService`; await it at
  boot so a bind failure is a boot failure (`GrpcServeError::Bind`, the
  `io::Error` as its source); `local_addr()` names the bound address (bind
  port `0` to let the kernel pick), then
  `tokio::spawn(bound.run(cancel))`. Once `cancel` fires, `run` closes the
  listener, sends every open connection an HTTP/2 GOAWAY, and returns
  when they have all closed or `GrpcLimits::drain_timeout` has passed,
  whichever is first. `run` returns `()`: a bound server has no failure of its
  own (accept errors back off and retry). Connections still open then are
  dropped with the runtime. The default `drain_timeout` is 10 s; zero returns
  at once.

### Adding a method

Every method lives in one registry, `ROUTES` in `service.rs`: one row per
method with its wire name (also its `method` metric label), its admission
`Class` (`Work`, or `Subscription` for a stream that idles between arrivals:
own pool, never cut for silence, no latency recorded) and the index whose view
its handler reads (`None` = reads none, so never disabled). A new RPC is:

1. a `Method` variant (rows stay in `Method` order: a const assertion checks it),
2. its `ROUTES` row,
3. its arm in `Routed::handle`, returning `Result<Response<Body>, Status>`
   (built with `wire::unary_response`, `wire::streamed_response` or
   `wire::streaming`, never by hand: `streaming` joins ready records into full
   DATA frames).

The path is resolved to its row once per request (`Method::resolve`, in the
outermost layer); admission, the silence bound, metrics labels and the
disabled-index answer all read that row; an `Err` becomes its status in one
place (`Wired::answer`).

### Why `GetTransaction` forwards

Raw transaction bytes are consensus data the validator already stores. Indexing
them would be a second copy of the chain, far larger than the compact store
(compact blocks drop proofs, signatures and 528 of each 580-byte note
ciphertext), to serve a read that happens only for transactions a wallet already
trial-decrypted. `GetAddressUtxos` and the other address methods are the
opposite case: they are *derived*, so forwarding them would mean a second
implementation that can disagree, or a dependency on a validator index Zebra
need not have.

`GetTransaction` (and the bytes behind `GetTaddressTransactions`) asks the
traffic balancer's trusted validators in turn: an absent answer moves to the next, and
the answer is `NOT_FOUND` only when every validator asked said absent (any other
failure = `UNAVAILABLE`). `height` = the mined height on the best chain, else `0`
(mempool, or mined on a branch no longer best). Only the `hash` form of
`TxFilter` is served.

## Serve stack

hyper-util's HTTP/2 server around the path dispatch, not
`tonic::transport::Server`, so the accept loop is zaino's:

Outermost first, per connection after the first three:

```text
accept ─ total cap ─ PROXY header (trusted peers) ─ per-client cap ─ TLS ─ h2 / HTTP/1.1
  ─ health (resolves the RPC) ─ metrics ─ admission ─ dispatch (grpc-timeout, silence) ─ handler
```

- **Health** (`with_health(Arc<dyn Health>)`): `GET /readyz` (`Health::readyz` → `200` / `503`)
  and `GET /status` (`Health::status`, always `200`) are answered on this listener over
  HTTP/1.1 or h2, before admission, so they never take a work slot.
  - TLS offers ALPN `h2` then `http/1.1`, so load-balancer monitors connect over HTTPS/1.1.
  - Each connection may send 10 at once and regains one per second; past that the answer is `429`.
  - A non-`GET` to either path is `405`. Any other path over HTTP/1.x is `404`.
  - Without `with_health`, those paths are like any other: `404` over HTTP/1.1, `UNIMPLEMENTED`
    over h2.
- **TLS** (`with_tls(Tls::load(files)?)`): rustls with the ring provider, ALPN `h2` then
  `http/1.1`. A missing or mismatched pair fails `Tls::load`, never plaintext. The files are
  checked every 60 s; a changed pair serves new connections, and one that fails to load is
  logged while the previous pair keeps serving. A handshake not done within 10 s closes the
  connection. A PROXY header, when required, precedes the handshake.
- An HTTP/1.1 connection must send its request headers within 10 s.
- `lightd_info(snap, network, version)` = `GetLightdInfo`'s answer off a snapshot, `version` the
  daemon's (`None` = `UNAVAILABLE`).

Every cap refuses; none queues except the read lanes:

| `GrpcLimits` field | Default | Enforced at | Over it |
|---|---|---|---|
| `max_connections` | 4096 | accept | socket closed |
| `max_connections_per_ip` | 32 | once the client is named | socket closed |
| `max_streams_per_connection` | 8 | h2 `SETTINGS`: every stream on the connection, any method | peer cannot open one |
| `max_work_streams` | 2048 | admission, every method but `GetMempoolStream` (the adaptive limit's ceiling) | `UNAVAILABLE` + `grpc-retry-pushback-ms: 250` |
| `adaptive_work_streams` | on | admission, same methods: the latency-adaptive limit (below) | same |
| `max_subscriptions` | 4096 | admission, `GetMempoolStream` | same |
| `max_point_reads` | 64 | one block / tree state / root list off the files | bounded wait |
| `max_range_reads` | 32 | one `GetBlockRange` window off the files | bounded wait |
| `max_scan_reads` | 4 | one address-history scan | bounded wait |
| `unread_timeout` | 300 s | per stream: data handed over, unread by the client | connection closed |
| `silent_timeout` | 300 s | per stream: nothing to send (not `GetMempoolStream`) | stream `UNAVAILABLE` |
| `drain_timeout` | 10 s | `run`, after `cancel` | still-open connections dropped |
| `max_connection_age` | 30 min ±10% | per connection | GOAWAY, then the grace |
| `max_connection_idle` | 30 min | per connection, no open stream | GOAWAY |
| `max_connection_age_grace` | 600 s | a GOAWAY'd connection's in-flight streams | connection dropped |

- **Connection age and idleness** (gRPC A9). Wallets hold one connection for
  hours, so without an age limit they never re-resolve DNS: neither a load
  balancer nor a replaced node moves them. At its age (drawn per connection
  within ±10%, so connections opened together do not cycle together) or after
  `max_connection_idle` with no open stream, a connection gets an HTTP/2 GOAWAY.
  Its in-flight streams then have `max_connection_age_grace` to finish before it
  closes. `None` turns either off. `retire()` → `Retire::retire_all()` does the
  same to every connection open at the call (an operator hold).
  `zaino_grpc_retired_connections_total{reason="age"|"idle"|"retired"}` counts
  all three.
- **Adaptive limit.** Work streams admitted at once follow measured latency,
  using Netflix concurrency-limits' Gradient2:
  - A sample is the time the server spent producing one message: from admission
    to the first DATA frame, then from each poll to the next. A client's
    backpressure never counts.
  - Each window (≥ 500 ms and ≥ 10 samples) compares its mean against a
    long-term baseline averaged over 600 windows:
    `limit × clamp(1.5 × baseline / mean, 0.5, 1) + √limit`, smoothed at 0.2.
  - The limit stays between 32 and `max_work_streams`, starting at 256, and
    admission holds work streams to it exactly.
  - A window that peaked below half the limit is app-limited and changes nothing.
  - Refusal is the same `UNAVAILABLE` + pushback. `adaptive_work_streams = false`
    keeps the limit at `max_work_streams`.
  - `zaino_grpc_adaptive_stream_limit` exports the limit.
- **Load report.** `GrpcService::admission()` → `AdmissionState::report()` gives
  `{limit, in_flight, refused_per_s, overloaded}`. `refused_per_s` and
  `overloaded` move once a minute, at the serving summary: `overloaded` sets
  once every minute for 5 minutes refused ≥ 10% of work (at ≥ 1/s), and clears
  once every minute for 5 minutes refused under 2% (`overload.rs`).
- A stream's slot is owned by the response body, so it returns when the stream
  ends or the client disconnects.
- A mempool subscription idles until the next block, one per connected wallet,
  so it never holds a work slot.
- `with_trusted_proxies(TrustedProxies::new(cidrs))`: a peer inside one of the
  networks must open with a PROXY header (v1 or v2, read under 5 s, ≤ 4 KiB), and
  the per-client cap counts the address it names. `LOCAL`/`UNKNOWN` headers
  count the proxy itself. A malformed or missing header closes the connection.
- Each accepted socket gets `TCP_NODELAY` and, on Linux, `TCP_NOTSENT_LOWAT` =
  128 KiB. Unsent bytes then wait in h2, not the kernel, so a unary reply is not
  stuck behind a range already queued on the same connection.
- An `accept()` error never ends `run`: a peer-side error is skipped, anything
  else (`EMFILE`, `ENOBUFS`, ...) backs off 10 ms doubling to 1 s.
- Request bodies are capped before decode: 64 KiB, or 2 MB + 1 KiB for
  `SendTransaction`. Over the cap is `RESOURCE_EXHAUSTED`. A body not complete
  within 30 s is `DEADLINE_EXCEEDED`.
- **No total cap.** A stream lives as long as it makes progress. Its bounds are
  the client's `grpc-timeout`, `unread_timeout` (client side) and
  `silent_timeout` (server side), below.
- **Deadline.** `grpc-timeout` (at most 8 digits plus `H`, `M`, `S`, `m`, `u` or
  `n`; a malformed value is ignored, as tonic ignores it) is read at arrival. Past
  it, a handler still pending answers `DEADLINE_EXCEEDED`, and a stream ends
  with `DEADLINE_EXCEEDED` trailers. The stream's body is dropped with it, so its
  reads and slot go too.
- **Unread.** A body that handed hyper a frame and is not polled again, because
  flow control stalled on a client that stopped reading, owes that frame. Once
  any stream on a connection has owed for `unread_timeout`, the connection is
  closed and `zaino_grpc_stalled_connections_total` counts it. An idle stream
  (a quiet mempool subscription, a read waiting for its lane) never owes.
- **Silence.** A handler or stream body with nothing to send for `silent_timeout`
  while hyper waits on it ends with `UNAVAILABLE` (`nothing to send for …`). The
  clock restarts at every frame. `GetMempoolStream` is exempt, because it is
  silent until the next block by design.
- **Frames.** Records that are ready back to back (`GetAddressUtxosStream`,
  `GetMempoolTx`, `GetBlockRange`'s in-memory blocks) are joined into DATA
  chunks of up to 16 KiB. A record of 16 KiB or more passes through uncopied,
  and nothing waits for more data. Sending one small frame per record makes an
  h2 0.4.16+ client that reads late answer `GOAWAY too_many_data_frames`
  (lightwalletd #593). `GetMempoolStream` keeps one frame per arrival, shared by
  every subscriber.

### Read lanes

`ReadLanes` (owned by the `GrpcService`, one per process) runs every read that
touches index files on the blocking pool, under a permit of its lane, never on
a runtime worker:

- **point**: block by height or hash, tree state, subtree roots
- **range**: each `GetBlockRange` window below the snapshot's layer/committed seam
- **scan**: address UTXOs, balances and transactions

A heavy scan queues behind the other scans, never behind or ahead of a tip read.
`zaino_grpc_disk_read_wait_seconds{lane}` is each lane's wait.

### Answered without a read

The answers every synced wallet asks for right after each block never queue:

| Answer | Why it needs no read |
|---|---|
| `GetLatestBlock` | the snapshot's tip |
| `GetBlock` by height, in the snapshot's layer | RAM record |
| `GetBlockRange`, in the layer | one RAM record per chunk, projected on read |
| `GetMempoolStream` opening | framed once per served-tip epoch |
| `GetTreeState` (layer heights), `GetLatestTreeState` | framed once per NFS publish |
| `GetSubtreeRoots` | each pool's list framed once per NFS publish; a request = one slice |

The tree-state memos (`memo::PerView`) are keyed on the NFS publish (the
`Indexed` `Arc`), not the global snapshot, which moves on every chain-view fold.
A new publish starts empty, so there is no invalidation rule. The first ask per
publish computes on the point lane, single-flight: the rest of the herd waits
for that one computation, then every later ask is answered inline.

- Per connection: 64 KiB per-stream send buffer, 30 s keepalive ping (20 s
  timeout), rapid-reset limit of 128.
- Metrics: `method` and `code` labels come from fixed tables (every
  `CompactTxStreamer` method plus `unknown`); each series handle is resolved once
  per process, never per request.

### Serving log

`run` logs a summary of each minute's traffic under the caller's span (connection
tasks inherit it too), built from the same close-outs the metrics count:

```text
INFO  … Grpc:  Serving    rps=42.1 p99=38.4ms out=3.25MB/s conns=17
WARN  … Grpc:  Serving    rps=14.7 p99=… failed=2 refused=31 at_capacity=29 slow=3 slowest=GetTaddressTxids
WARN  … Grpc:  High load  rps=412 p99=1.20s out=6.04MB/s conns=1,203/4,096 work=640/2,048
ERROR … Grpc:  Request failed    method=GetBlock code=DataLoss error="…"
```

- **Rate** = streams finished in the window, per second. **Latency** = time to
  the first message of each `OK` work request (a unary answer, or a stream's
  first item; `GetMempoolStream` is never timed), p50 / p99 from log-linear
  buckets ≤ 12.5 % wide, never above `max`, which is exact. **out** = response
  body bytes (gRPC frames, before HTTP/2 and TCP overhead). **work / subs /
  conns** = work streams, subscriptions and connections held at the summary, of
  their caps.
- Status codes by who they blame: `Internal`, `Unknown`, `DataLoss` = **failed**
  (this server); `Unavailable`, `ResourceExhausted` = **refused** (at capacity,
  nothing served yet, validator unreachable; `at_capacity` = the admission share);
  everything else = the client's (bad argument, not found, cancelled), counted
  per method at DEBUG only.
- The summary is WARN once the window holds a failure, a refusal, a request
  slower than 1 s to its first message (`slow`, `slowest` = the method with the
  longest), a stalled connection closed (`stalled`) or a connection refused at a
  cap (`conns_refused`); problem fields appear only when non-zero. Nothing
  served and nothing held = no line.
- While more than 25% of the work-stream, subscription or connection cap is held, the
  summary is WARN `High load` instead, with each such cap as `used/max`.
- The window's first failure is logged as it happens (ERROR `Request failed`:
  `method`, `code`, the `error` message the client got), and its first
  refusal other than admission (WARN `Request unavailable`); the rest are only
  counted, so an outage costs two lines a minute, not one per request.
- DEBUG adds `Method served` per method with traffic: `requests`, `p50`, `p99`,
  `out`, `client`, `refused`, `failed`.
- Cost: a few relaxed atomic adds per request into fixed per-method counters,
  swapped to zero once a minute on the accept loop; no lock, no allocation per
  request.

## Stored bytes on the wire

Compact-block records are stored gRPC-framed (`[0x00][len:be32][message]`), so
`GetBlock` and `GetBlockRange` write the index's bytes (pool-pruned by its
`RangeCursor`) straight into the response body: a unary response is one
record, a stream is records concatenated. Range steps below the seam run on the
blocking pool under a range-lane permit. Only `GetLatestBlock` (a two-field `BlockId`) and
the deprecated `GetBlockRangeNullifiers` re-projection encode messages.
Tree-state, transparent-address and chain-view answers are domain values; their
dispatch builds the proto message and frames it.

`GetMempoolStream` opens with its epoch's whole opening as one DATA chunk. The
chunk is framed once per served-tip epoch and shared by refcount, so the
per-block reconnect of every wallet costs one render. Each later arrival is
its own record. With no held verified tip the stream is `UNAVAILABLE`, never opened silent.
