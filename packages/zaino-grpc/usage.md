# zaino-grpc

The wallet-facing `CompactTxStreamer` endpoint: one `GrpcService`, every method dispatched by
path over `RouteDeps`. What answers each method, and the status codes a client must tell apart:
[`docs/rpc_api.md`](../../docs/rpc_api.md).

## RouteDeps

```rust
let routes = RouteDeps {
    grpc_snapshots: publisher.handle(),  // zaino_grpc_snapshot::GrpcSnapshots<V>: one load per request
    submit: Arc::clone(&view),           // Arc<ChainView<S>>: SendTransaction
    validators,                          // zaino_traffic::TrafficBalancer<S>: GetTransaction
    network,                             // declared (GetLightdInfo.chainName)
    version: env!("CARGO_PKG_VERSION"),  // the daemon's (GetLightdInfo.version)
    max_address_rows,                    // receives one t-address request may walk
    indexes,                             // Vec<IndexKind>: the configured ones
};
let bound = GrpcService::new(grpc_listen_address, limits)  // limits.trusted_proxies included
    .with_routes(routes)
    .with_retire(retire)                 // the owner's operator hold (zaino_grpc::Retire)
    .with_tls(tls)
    .bind()
    .await?;
tokio::spawn(bound.run(cancel.child_token()));
```

A server can bind and run before its routes exist (an owner still booting or downloading an
index snapshot): it answers health from the start and every method `UNAVAILABLE`
(`starting: not routed yet`). Take its `routing()` handle before `bind()` and call
`route(routes)` once they exist. Routing twice panics.

```rust
let server = GrpcService::new(grpc_listen_address, limits).with_health(health);
let routing = server.routing();
tokio::spawn(server.bind().await?.run(cancel.child_token()));
// … boot …
routing.route(routes);
```

- **One `GrpcSnapshot` per request** (G1): each handler loads one and reads only it for the
  request or the whole stream (`served()?`, `indexed()` for tree states, `mempool()?`,
  `lightd()?`). Exceptions: `SendTransaction` (`chain_view.validate_transaction` →
  `broadcast_transaction`) and `GetTransaction` (`validators.lookup`, then the served mempool's
  bytes, else the peers' block the txid-location index names).
- No height named = the served tip. A height named = up to `IndexesAtBlock::answers_through(kind)`:
  a point read past it is `NOT_FOUND`, a range streams to it, then `OUT_OF_RANGE`.
- By `BlockID.hash`: `height_of(hash)` in the `block_hash` view names the height, and the
  answering index must hold the same hash there (else `NOT_FOUND`). Block-hash disabled = a hash
  request is `UNIMPLEMENTED`.
- An index missing from `RouteDeps::indexes` = disabled: its methods are `UNIMPLEMENTED` naming
  it, served or not. An unknown path is `UNIMPLEMENTED`.
- `zaino_grpc_snapshot::Unavailable` → `UNAVAILABLE` with its message.
- `S` = the validators' `ChainDataSource`; `V` = the indexes' `zaino_persistence::PinnedView`
  (zainod: `DiskView`).
- `limits: GrpcLimits` is zainod's `[grpc]` table as parsed (unknown keys refused).
- `bind().await` takes the socket (a failure = `GrpcServeError::Bind`, so a boot failure);
  `local_addr()` names it (bind port `0` to let the kernel pick). Once `cancel` fires, `run`
  closes the listener, sends every connection a GOAWAY, and returns when they have all closed or
  `drain_timeout` has passed. `run` has no failure of its own: accept errors back off and retry.
- `chainName`, the tree state's `network` and the t-address mismatch message spell the network
  as lightwalletd does (`main` / `test` / `regtest`), from `routes::info::chain_name`.

### Adding a method

Every method is one `Method` variant in `service.rs`; its name is its wire name and its `method`
metric label. A new RPC is:

1. a `Method` variant,
2. its arm in `Method::route`: its admission `Class` (`Work`, or `Subscription` for a stream
   that idles between arrivals: own pool, never cut for silence, no latency recorded) and the
   index whose view its handler reads (`None` = reads none, so never disabled),
3. its arm in `Wired::handle`, which hands the handler its decoded request (`wire::decode`, or
   `wire::decode_each` for a client stream); the handler returns `Result<Response<Body>, Status>`
   built with `wire::unary_response`, `wire::streamed_response` or `wire::streaming`, never by
   hand (`streaming` joins ready records into full DATA frames).

Both matches are exhaustive, so the compiler names the arms a new variant still needs. An `Err`
becomes its status in one place (`Wired::answer`).

### Why `GetTransaction` forwards

Raw transaction bytes are consensus data the validator already stores. Indexing them would be a
second copy of the chain, far larger than the compact store (compact blocks drop proofs,
signatures and 528 of each 580-byte note ciphertext), to serve a read that happens only for
transactions a wallet already trial-decrypted. The address methods are the opposite case: they
are *derived*, so forwarding them would mean a second implementation that can disagree, or a
dependency on a validator index Zebra need not have. The path with no validator answering:
[architecture §1](../../docs/architecture.md#1-who-owns-what).

## Serve stack

hyper-util's HTTP/2 server around the path dispatch, not `tonic::transport::Server`, so the
accept loop is zaino's. Outermost first, per connection after the first three:

```text
accept ─ total cap ─ PROXY header (trusted peers) ─ per-client cap ─ TLS ─ h2 / HTTP/1.1
  ─ health (resolves the RPC) ─ guard (metrics, admission, stall watch)
  ─ dispatch (grpc-timeout, silence, request body) ─ handler
```

The guard (`guard.rs`) wraps each response body once: the body owns its admission slot, its
stall-watch ticket and its close-out metrics, so all three end together when hyper drops it (a
stream ending or its client disconnecting). What each `[grpc]` cap bounds:
[the example config](../../docs/example_configs/zainod.toml).

- **Health** (`with_health(Arc<dyn Health>)`): `GET /readyz` (`200` / `503`) and `GET /status`
  (always `200`), answered before admission over HTTP/1.1 or h2, so they never take a work slot.
  Each connection may send 10 at once and regains one per second; past that `429`. A non-`GET`
  is `405`, any other path over HTTP/1.x `404`.
- **TLS** (`with_tls(Tls::load(files)?)`): rustls with the ring provider, ALPN `h2` then
  `http/1.1` (load-balancer monitors connect over HTTPS/1.1). A missing or mismatched pair fails
  `Tls::load`, never plaintext. The files are checked every 60 s; a changed pair serves new
  connections, one that fails to load is logged while the previous pair keeps serving.
- **Connection age and idleness** (gRPC A9). Wallets hold one connection for hours, so without
  an age limit they never re-resolve DNS. At its age (drawn per connection within ±10%, so
  connections opened together do not cycle together) or once idle, a connection gets a GOAWAY;
  its in-flight streams then have `max_connection_age_grace`.
- **Operator hold** (`Retire`, the owner's, handed in with `with_retire`, so a hold may start
  before the server exists). `Retire::hold(delay)`: once `delay` has passed, every connection
  gets the same GOAWAY + grace, those open then and each one accepted while held. A repeated
  `hold` keeps the first deadline; `resume()` lifts it. `Retire::state()` = `Hold::Off`,
  `Draining` or `Drained`.
- **Adaptive limit.** Work streams admitted at once follow measured latency (Netflix
  concurrency-limits' Gradient2). A sample is the server's time to produce one message (admission
  to the first DATA frame, then poll to poll), so a client's backpressure never counts. Each
  window (≥ 500 ms, ≥ 10 samples) sets `limit × clamp(1.5 × baseline / mean, 0.5, 1) + √limit`,
  smoothed at 0.2, between 32 and `max_work_streams`; a window that peaked below half the limit
  changes nothing.
- **Load report.** `GrpcService::admission()` → `AdmissionState::report()` gives
  `{limit, in_flight, refused_per_s, overload}`, moved once a minute.
- **Trusted proxies.** A peer inside `trusted_proxies` must open with a PROXY header (v1 or v2,
  read under 5 s, ≤ 4 KiB), and the per-client cap counts the address it names; `LOCAL` /
  `UNKNOWN` count the proxy itself.
- Each accepted socket gets `TCP_NODELAY` and, on Linux, `TCP_NOTSENT_LOWAT` = 128 KiB: unsent
  bytes wait in h2, not the kernel, so a unary reply is not stuck behind a range already queued
  on the same connection.
- Request bodies are capped before decode ([sizes](../../docs/rpc_api.md#status-codes-a-client-must-distinguish)).
- **Deadline.** `grpc-timeout` is read at arrival (a malformed value is ignored, as tonic
  ignores it). Past it a pending handler answers `DEADLINE_EXCEEDED` and a stream ends with
  `DEADLINE_EXCEEDED` trailers; its reads and slot go with it.
- **Unread.** A body that handed hyper a frame and is not polled again (flow control stalled on
  a client that stopped reading) owes that frame. Once any stream on a connection has owed for
  `unread_timeout`, the connection is closed. An idle stream (a quiet mempool subscription, a
  read waiting for its lane) never owes.
- **Silence.** A handler or stream body with nothing to send for `silent_timeout` ends with
  `UNAVAILABLE`; the clock restarts at every frame. `GetMempoolStream` is exempt.
- **Frames.** Records ready back to back are joined into DATA chunks of up to 16 KiB, and a
  record of 16 KiB or more passes through uncopied. One small frame per record makes an h2
  0.4.16+ client that reads late answer `GOAWAY too_many_data_frames` (lightwalletd #593).
- `method` and `code` metric labels come from fixed sets, so a misbehaving client cannot grow
  the series count.

### Read lanes

`ReadLanes` runs every read that touches index files on the blocking pool, under a permit of its
lane, never on a runtime worker: **point** (block, tree state, subtree roots), **range** (each
`GetBlockRange` window below the NFS layer), **scan** (address UTXOs, balances, transactions). A
heavy scan queues behind the other scans, never behind or ahead of a tip read.

### Answered without a read

The answers every synced wallet asks for right after each block never queue:

| Answer | Why it needs no read |
|---|---|
| `GetLatestBlock` | the served tip |
| `GetBlock` by height, in the NFS layer | RAM record |
| `GetBlockRange`, in the layer | one RAM record per chunk, projected on read |
| `GetMempoolStream` opening | framed once per served-tip epoch |
| `GetTreeState` (layer heights), `GetLatestTreeState` | framed once per NFS publish |
| `GetSubtreeRoots` | each pool's list framed once per NFS publish; a request = one slice |

The tree-state memos (`memo::PerView`) are keyed on the NFS publish (the `NfsPublication` `Arc`),
not the `GrpcSnapshot`, which moves on every chain-view fold. A new publish starts empty, so
there is no invalidation rule. The first ask per publish computes on the point lane,
single-flight; every later ask is answered inline.

### Overload

`report().overload` names the first cause latched (`/readyz`: `Overloaded: <cause>`). A cause
latches once over its `[grpc.overload]` threshold (`OverloadLimits`, 0 = off) for its enter time
and clears once under half of it for its exit time; `AdmissionState::set_overload` changes the
thresholds live.

| Cause | Signal, per one-minute window | Enter / exit |
| ----- | ----------------------------- | ------------ |
| `network saturated` | 20th percentile of `zaino_grpc_queue_delay_seconds` (RTT over the client network's base RTT: min over 10 min of every socket, as LEDBAT) or of `zaino_grpc_retransmit_ratio` (`network_retransmit_percent`); one per client /24 or /48 every 10 s (this listener's sockets, sock_diag), over samples with ≥ `network_min_networks` networks | 2 / 3 min |
| `CPU pressure`, `IO pressure` | share of the window zainod's cgroup waited (Linux PSI) | 2 / 3 min |
| `refusing work` | admission refused ≥ 10% of work at ≥ 1/s (exit: < 2%) | 5 / 5 min |

### Serving log

`run` logs one `Serving` summary a minute under the caller's span, from the same close-outs the
metrics count: `rps` = streams finished per second, `conns` = connections held. It is WARN once
the window holds a failure, a refusal, a request slower than 1 s to its first message (`slow`),
a stalled connection closed (`stalled`) or a connection refused at a cap (`conns_refused`); those
fields appear only when non-zero. Nothing served and nothing held = no line.

- Status codes by who they blame: `Internal`, `Unknown`, `DataLoss` = **failed** (this server);
  `Unavailable`, `ResourceExhausted` = **refused** (`at_capacity` = the admission share);
  everything else = the client's, left to `requests_total{code}`.
- The window's first failure is logged as it happens (ERROR `Request failed`: `method`, `code`,
  the client's `error`), and its first refusal other than admission (WARN `Request unavailable`);
  the rest are only counted, so an outage costs two lines a minute.
- Latency metrics by each `Method`'s `Reply` (a test holds every method to `service.proto`): a
  unary reply records `duration_seconds` (`OK` answers only), a server stream
  `first_message_seconds`.

## Stored bytes on the wire

Compact-block records are stored gRPC-framed (`[0x00][len:be32][message]`), so `GetBlock` and
`GetBlockRange` write the index's bytes (pool-pruned by its `RangeCursor`) straight into the
response body. Only `GetLatestBlock` and the deprecated `GetBlockRangeNullifiers` re-projection
encode messages. Tree-state, transparent-address and chain-view answers are domain values,
framed by their dispatch. `GetMempoolStream`'s opening is one DATA chunk, framed once per
served-tip epoch and shared by refcount, so the per-block reconnect of every wallet costs one
render; each later arrival is its own record.
