# zaino-lightserve

The lightwalletd-compatible `CompactTxStreamer` gRPC adapter. It renders the
answers of a `LightWalletService` engine onto the lightwalletd wire; it owns the
domain → wire conversions and nothing about where an answer comes from.

## Wiring

- `LightServe::new(engine, network)` builds the handler. The network is a serving
  parameter: `TreeState.network` and `GetLightdInfo.chainName` are functions of
  the chain, not of any block read.
- `LightServe::with_upgrades(schedule)` carries the validator's network upgrade
  schedule (zainod reads it at boot with `zaino_runtime::read_upgrade_schedule`).
  `GetLightdInfo` reports `saplingActivationHeight`, `consensusBranchId` (the
  branch in force at the served tip, as lightwalletd reports
  `consensus.chaintip`) and `upgradeName`/`upgradeHeight` for the next pending
  upgrade from it; without one those fields are empty.
- `GrpcServer::new(handler, bind)` serves it as a supervised run loop. Accepted
  connections have Nagle disabled.

## Serving contracts

- `GetBlockRange` / `GetBlockRangeNullifiers` stream the range in steps of 100
  heights over one pinned snapshot, so an open stream holds at most one step in
  memory. A range whose start is above its end is served highest first, as
  lightwalletd does.
- `GetTreeState` reads a request as lightwalletd does: a non-zero height selects
  by height; otherwise a 32-byte hash, in display order, selects the block on the
  served chain (`NOT_FOUND` when no block there has it). Tree states carry the
  serialized trees only; the wire has no roots, so the adapter never asks for them.

## Metrics (`prometheus` feature)

The wire layer of [Zaino's metrics](../../docs/observability.md), which holds
the principles, layering, conventions and hot-path budget. Off by default. With the feature, `GrpcServer` wraps the service in a measuring
layer: the clock starts when a request reaches the server, the first response
DATA frame records time-to-first-message, and the call closes out when its
response body drops, so a client that disconnects mid-stream is counted as
`Cancelled` rather than lost.

| Metric | Kind | Labels |
|---|---|---|
| `zaino.grpc.requests_total` | counter | `method`, `code` |
| `zaino.grpc.first_message_seconds` | histogram | `method` |
| `zaino.grpc.duration_seconds` | histogram | `method` |
| `zaino.grpc.stream_messages` | histogram | `method` |
| `zaino.grpc.sent_bytes_total` | counter | `method` |

`method` is a `CompactTxStreamer` method name or `unknown`; `code` is one of the
17 gRPC status codes.
