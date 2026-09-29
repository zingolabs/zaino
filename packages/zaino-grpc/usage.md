# zaino-grpc

The wallet-facing `CompactTxStreamer` gRPC server. One service; each method is
answered by an index, the chain view, or the validator. The per-method table and
the status codes a client must distinguish live in
[`docs/rpc_api.md`](../../docs/rpc_api.md).

## Routing

`CompactTxStreamer` is one fixed service name, so its methods cannot be split
across tonic services. [`Router`] is the named service and dispatches by method
path:

- a path claimed by a wired index or the chain view is answered there;
- everything else falls through to [`GrpcService`], the generated server, which
  answers `GetTransaction` and `GetLightdInfo` off the validator and
  `UNIMPLEMENTED` for the rest.

`ValidatorPorts` is the validator's whole surface here:
`OneShotSendRawTransaction + OneShotGetTransaction + OneShotGetBlockchainInfo`
(from `zaino-source`). Derived answers have no port to forward to, so they
cannot silently fall back to the validator — see
[`docs/design/boundaries.md`](../../docs/design/boundaries.md).

`GetTaddressTransactions` (and its deprecated alias `GetTaddressTxids`) needs
both halves: the transparent index names the transactions, the validator holds
the bytes. `GrpcServer::new` wires the validator half automatically.

## Building a server

```rust
let serve = GrpcServer::new(
    ValidatorHandler::new(Arc::clone(&validator), compact_block_service.clone(), network),
    grpc_listen_address,
    limits,
)
.with_compact_block(compact_block_service)
.with_tree_state(tree_state_service)
.with_transparent_address(transparent_service)
.with_chainview(chainview_handles)
.bind()
.await?;
tokio::spawn(serve.run(cancel.child_token()));
```

- `ValidatorHandler::new(source: Arc<S>, served: CompactBlockService,
  network: NetworkType)` — `served` gives `LightdInfo.block_height` (the
  compact-block index's tip); `network` is the configured network, never read
  off the validator.
- `limits: GrpcLimits` (see [Serve stack](#serve-stack)); zainod fills it from
  its `[grpc]` config section.
- Every `with_*` is optional. An omitted index leaves its paths with
  `GrpcService`, which answers `UNIMPLEMENTED`.
- `GetBlock` and `GetTreeState` by `BlockID.hash` resolve through
  `with_block_hash(BlockHashService)`: its `locate(hash)` names the height, and
  the answering index must hold the same hash there (else `NOT_FOUND`).
  Without the block-hash index wired, a hash request is `UNIMPLEMENTED`.
- A t-address encoded for a network other than the transparent index's is
  `INVALID_ARGUMENT`.
- `ChainViewHandles { view, relay, compact }` backs `SendTransaction` (relayed
  to every validator via the `Relay` port), `GetMempoolTx` and
  `GetMempoolStream`. `ProjectCompact` renders raw mempool bytes as a
  `CompactTx`; zainod supplies the implementation.
- `GrpcServer::bind().await` takes the socket and returns a
  `BoundGrpcServer`; await it at boot so a bind failure is a boot failure
  (`GrpcServeError::Serve`), then `tokio::spawn(bound.run(cancel))`. `run`
  returns `Ok(())` once `cancel` fires, open connections shutting down
  gracefully.

## Features

| Feature | Default | Effect |
|---|---|---|
| `index-compact-block` | on | claims `GetLatestBlock`, `GetBlock`, `GetBlockRange`, `GetBlockRangeNullifiers` |
| `index-tree-state` | on | claims `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` |
| `index-transparent-address` | on | claims `GetAddressUtxos[Stream]`, `GetTaddressBalance[Stream]`, `GetTaddressTransactions`, `GetTaddressTxids` |
| `chainview` | on | claims `SendTransaction`, `GetMempoolTx`, `GetMempoolStream` |

Off means the paths are not claimed and the index crate is not compiled in.

## Serve stack

hyper-util's HTTP/2 server with the routed tonic service mounted, not
`tonic::transport::Server`, so the accept loop is zaino's:

```text
accept ─ total cap ─ client identity ─ per-client cap ─ h2 conn ─ admission ─ Router
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
| `stall_timeout` | 300 s | per connection | connection closed |

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
- `GetLightdInfo` reuses one `getblockchaininfo` for up to 1 s. Concurrent
  callers share the one in flight, and a failure is never kept.
- **Stalls.** A body that handed hyper a frame and is not polled again, because
  flow control stalled on a client that stopped reading, owes that frame. Once
  any stream on a connection has owed for `stall_timeout`, the connection is
  closed and `zaino_grpc_stalled_connections_total` counts it. An idle stream
  (a quiet mempool subscription, a read waiting for its lane) never owes.

### Read lanes

`ReadLanes` (owned by the `Router`, one per process) runs every read that
touches index files on the blocking pool, under a permit of its lane, never on
a runtime worker:

- **point**: block by height or hash, tree state, subtree roots
- **range**: each `GetBlockRange` window below the finalised seam
- **scan**: address UTXOs, balances and transactions

A heavy scan queues behind the other scans, never behind or ahead of a tip read.
`zaino_grpc_disk_read_wait_seconds{lane}` is each lane's wait.

### Answered without a read

The answers every synced wallet asks for right after each block never queue:

| Answer | Why it needs no read |
|---|---|
| `GetLatestBlock` | tip height + hash resolved when the view was published |
| `GetBlock` by height, nonfinalised | RAM record |
| `GetBlockRange`, nonfinalised, default pools | projection stored at apply (zero-copy) |
| `GetMempoolStream` snapshot | framed once per chain-view publication |
| `GetTreeState` (nonfinalised heights), `GetLatestTreeState` | framed once per publication |
| `GetSubtreeRoots` | each pool's list framed once per publication; a request = one slice |

The per-publication memos (`memo::PerView`) are keyed on the pinned view's
`Arc`. A new publication starts empty, so there is no invalidation rule. The
first ask per publication computes on the point lane, single-flight: the rest
of the herd waits for that one computation, then every later ask is answered
inline.

- Per connection: 64 KiB per-stream send buffer, 30 s keepalive ping (20 s
  timeout), rapid-reset limit of 128.
- Metrics: `method` and `code` labels come from fixed tables (every
  `CompactTxStreamer` method plus `unknown`); each series handle is resolved once
  per process, never per request.

## Stored bytes on the wire

Compact-block records are stored gRPC-framed (`[0x00][len:be32][message]`), so
`GetBlock` and `GetBlockRange` write the index's bytes (pool-pruned by its
`RangeCursor`) straight into the response body: a unary response is one
record, a stream is records concatenated. Range steps below the seam run on the
blocking pool under a range-lane permit. Only `GetLatestBlock` (a two-field `BlockId`) and
the deprecated `GetBlockRangeNullifiers` re-projection encode messages.
Tree-state, transparent-address and chain-view answers are domain values; their
dispatch builds the proto message and frames it.

`GetMempoolStream` opens with the tail's whole snapshot as one DATA chunk. The
chunk is framed once per published chain view and shared by refcount, so the
per-block reconnect of every wallet costs one render. Each later arrival is
its own record. Below quorum the stream is `UNAVAILABLE`, never opened silent.
