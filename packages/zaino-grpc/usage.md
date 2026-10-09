# zaino-grpc

The wallet-facing `CompactTxStreamer` endpoint: one [`GrpcService`], every method
dispatched by path over [`Routes`]. Every method that reads chain, index, mempool or
validator state loads one `zaino_snapshot::Snapshot` and reads it for the life of its
request or stream. The per-method table and the status codes a client must distinguish
live in [`docs/rpc_api.md`](../../docs/rpc_api.md).

## Routes

```rust
let routes = Routes {
    snapshots: publisher.handle(),       // zaino_snapshot::Snapshots<V>: one load per request
    submit: Arc::clone(&view),           // Arc<ChainView<S>>: SendTransaction
    validators,                          // zaino_traffic::TrafficBalancer<S>: GetTransaction
    network,                             // declared (GetLightdInfo.chainName)
    max_address_rows,                    // receives one t-address request may walk
};
let bound = GrpcService::new(routes, grpc_listen_address, limits)
    .with_trusted_proxies(proxies)
    .with_tls(tls)
    .bind()
    .await?;
tokio::spawn(bound.run(cancel.child_token()));
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
| `GetLightdInfo` | `lightd()?`: a holder's `getblockchaininfo` + the served height; `network` as configured |
| `SendTransaction` | `submit` (chain view submission §6; no snapshot) |
| `GetTransaction` | `validators.transaction(txid)` (absent → the next; `NOT_FOUND` only when every one asked said absent) |

- **One snapshot per request** (`snapshots.load()`, pinned for the request or
  the whole stream, G1). No height named = the served tip, which every enabled
  index holds (R12): `GetLatestBlock`, `GetLatestTreeState`,
  `GetLightdInfo.blockHeight`, subtree roots completing at or below it, the
  transparent reader `.as_of(tip)`. A height named (`GetBlock`, `GetBlockRange`,
  `GetTreeState`, by height or hash) = up to `At::answers_through(kind)`, the
  index's own durable tip when past the served one. A point read past it =
  `NOT_FOUND`; a range streams to it, then `OUT_OF_RANGE`.
- Wire contract (`docs/design/integration-test-plan.md` W1–W15): `SendTransaction`
  rejections = gRPC `OK` + the validator's code and message; `GetTreeState` = the
  exact height, `""` below a pool's activation, `000000` for an empty active
  tree; `GetSubtreeRoots` whole or refused before the first byte (unknown pool
  `UNIMPLEMENTED`, `startIndex` past the end = empty `OK`). See
  [`docs/rpc_api.md`](../../docs/rpc_api.md).
- `Unavailable` = `UNAVAILABLE` with its message: nothing served yet (a booting
  NFS), no verified chain, or no trusted validator holding the best block (the
  mempool methods and `GetLightdInfo`).
- `GetMempoolStream` ends once the **served** tip moves (after the NFS folds the
  new block), so the `GetLatestBlock` a wallet sends after the end already
  answers the new tip.
- An index the snapshot lacks (`at.views().x()` = `None`) = disabled: its
  methods are `UNIMPLEMENTED`, naming the index (`GetTreeState needs the
  tree_state index, which is not enabled`). Compact-block is optional like the
  rest. An unknown path is `UNIMPLEMENTED`.
- `S` = the validators' `ChainDataSource`. Derived answers have no validator
  method to fall back to: they come from an index or not at all — see
  [`docs/design/boundaries.md`](../../docs/design/boundaries.md).
- `V` = the indexes' `zaino_persistence::CommittedView` (zainod: `DiskView`): every read = an
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
- `GetLightdInfo` never calls a validator: one snapshot load (`lightd()`); no
  holder or nothing served = `UNAVAILABLE`.
- `GetMempoolTx` parses each mempool transaction once (`decode_transaction` +
  `compact_tx`, cached on the entry's `Projection`); each request only renumbers
  slots and prunes pools.
- A t-address encoded for a network other than the transparent index's is
  `INVALID_ARGUMENT`.
- `limits: GrpcLimits` (see [Serve stack](#serve-stack)); zainod fills it from
  its `[grpc]` config section.
- `bind().await` takes the socket and returns a `BoundGrpcService`; await it at
  boot so a bind failure is a boot failure (`GrpcServeError::Serve`), then
  `tokio::spawn(bound.run(cancel))`. Once `cancel` fires, `run` closes the
  listener, sends every open connection an HTTP/2 GOAWAY, and returns `Ok(())`
  when they have all closed or `GrpcLimits::drain_timeout` has passed,
  whichever is first. Connections still open then are dropped with the runtime.
  The default `drain_timeout` is zero: return at once.

### Why `GetTransaction` forwards

Raw transaction bytes are consensus data the validator already stores. Indexing
them would be a second copy of the chain, about ten times the compact store
(compact blocks drop proofs, signatures and 528 of each 580-byte ciphertext), to
serve a read that happens only for transactions a wallet already trial-decrypted.
`GetAddressUtxos` and the other address methods are the opposite case: they are
*derived*, so forwarding them would mean a second implementation that can
disagree, or a dependency on a validator index Zebra need not have.

## Serve stack

hyper-util's HTTP/2 server around the path dispatch, not
`tonic::transport::Server`, so the accept loop is zaino's:

```text
accept ─ total cap ─ client identity ─ per-client cap ─ h2 conn ─ admission ─ dispatch
```

Every cap refuses; none queues except the read lanes:

| `GrpcLimits` field | Default | Enforced at | Over it |
|---|---|---|---|
| `max_connections` | 4096 | accept | socket closed |
| `max_connections_per_ip` | 32 | once the client is named | socket closed |
| `max_streams_per_connection` | 8 | h2 `SETTINGS` | peer cannot open one |
| `max_streams` | 2048 | admission, every method but `GetMempoolStream` | `UNAVAILABLE` + `grpc-retry-pushback-ms: 250` |
| `max_subscriptions` | 4096 | admission, `GetMempoolStream` | same |
| `max_point_reads` | 64 | one block / tree state / root list off the files | bounded wait |
| `max_range_reads` | 32 | one `GetBlockRange` window off the files | bounded wait |
| `max_scan_reads` | 4 | one address-history scan | bounded wait |
| `stall_timeout` | 300 s | per stream: unread data / nothing to send | connection closed / stream `UNAVAILABLE` |
| `drain_timeout` | 0 s | `run`, after `cancel` | still-open connections dropped |

- A stream permit is owned by the response body, so it returns when the stream
  ends or the client disconnects.
- A mempool subscription idles until the next block, one per connected wallet,
  so it never holds a work permit.
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
- `GetLightdInfo` never waits on a validator: its validator half is the chain
  view's last poll tick (≈1 s old at most while polling succeeds).
- **No total cap.** A stream lives as long as it makes progress. Its bounds are
  the client's `grpc-timeout` and `stall_timeout` without progress on either
  side, below.
- **Deadline.** `grpc-timeout` (at most 8 digits plus `H`, `M`, `S`, `m`, `u` or
  `n`; a malformed value is ignored, as tonic ignores it) is read at arrival. Past
  it, a handler still pending answers `DEADLINE_EXCEEDED`, and a stream ends
  with `DEADLINE_EXCEEDED` trailers. The stream's body is dropped with it, so its
  reads and permits go too.
- **Stalls.** A body that handed hyper a frame and is not polled again, because
  flow control stalled on a client that stopped reading, owes that frame. Once
  any stream on a connection has owed for `stall_timeout`, the connection is
  closed and `zaino_grpc_stalled_connections_total` counts it. An idle stream
  (a quiet mempool subscription, a read waiting for its lane) never owes.
- **Idle.** A handler or stream body with nothing to send for `stall_timeout`
  while hyper waits on it ends with `UNAVAILABLE` (`nothing to send for …`). The
  clock restarts at every frame. `GetMempoolStream` never idles out, because it
  is silent until the next block by design.
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

The per-snapshot memos (`memo::PerView`) are keyed on the snapshot's `Arc`. A
new snapshot starts empty, so there is no invalidation rule. The first ask per
snapshot computes on the point lane, single-flight: the rest
of the herd waits for that one computation, then every later ask is answered
inline.

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
WARN  … Grpc:  High load  rps=412 p99=1.20s out=6.04MB/s conns=1,203/4,096 streams=640/2,048
ERROR … Grpc:  Request failed    method=GetBlock code=DataLoss error="…"
```

- **Rate** = streams finished in the window, per second. **Latency** = time to
  the first message of each `OK` work request (a unary answer, or a stream's
  first item; `GetMempoolStream` is never timed), p50 / p99 from log-linear
  buckets ≤ 12.5 % wide, never above `max`, which is exact. **out** = response
  body bytes (gRPC frames, before HTTP/2 and TCP overhead). **streams / subs /
  conns** = permits and connections held at the summary, of their caps.
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
