# `zainod` — usage

The binary and `zainodlib`. Covers the admin listener (`metrics_endpoint`, feature `prometheus`);
configuration and serving live in the README.

## The admin listener

Own thread + current-thread runtime (a probe on the saturated serving runtime measures its queue; a
timed-out liveness probe kills the pod).

| Path       | Answers                                | `503` when                   |
| ---------- | -------------------------------------- | ---------------------------- |
| `/metrics` | Prometheus exposition: quantities only | never (render panic → `500`) |
| `/livez`   | indexer loop still running             | no heartbeat for 30s         |
| `/readyz`  | index synced, gRPC and JSON-RPC bound  | still syncing (body `syncing`) |

- Heartbeat republished by the indexer loop every 100ms

## Nothing is served until the index has synced

zainod binds its gRPC and JSON-RPC listeners only once the finalised state
reaches the finalised floor, which is the validator tip less the chain-head
depth. Until then no client can connect, and `/readyz` answers `503`. The
admin listener binds at startup, so `/livez` and `/metrics` work throughout
the sync. A supervisor restart clears readiness, and the restarted indexer
binds its listeners again once it has synced. Once bound, the listeners stay
up even if the index later falls behind.

### `/readyz` per component — TODO

Today `/readyz` answers only the sync gate above. The planned body:

Status code: `200` iff every component `Ready` + `Healthy`. Body, per component (`zaino-component`
`ComponentStatus`):

| Field       | Values                                                  |
| ----------- | ------------------------------------------------------- |
| `name`      | chain index, finalised state, chain head, mempool, gRPC, JSON-RPC |
| `lifecycle` | `Offline`, `Spawning`, `Syncing`, `Ready`, `Closing`    |
| `health`    | `Healthy`, `Recoverable`, `Critical`, `Offline`         |

Per-component detail:

| Component       | Detail                                                                                          |
| --------------- | ----------------------------------------------------------------------------------------------- |
| finalised state | building or synced; accumulator rebuild running                                                  |
| mempool         | completeness: `complete`, `incomplete(capacity_limited / pending_metadata / source_error)`      |

## Quantities on `/metrics`, modes on `/readyz`

- Metric = a quantity (count, height, duration, size)
- Mode as a gauge (`0 none, 1 read-only, 2 full`) → averaged, `rate()`d, thresholded: meaningless,
  never an error

## Metric names

- Declared by the emitting crate's `metric_names` module: `const` names + `COUNTERS` / `GAUGES` /
  `HISTOGRAMS` (`# HELP`) tables
- `zainod` registers them and owns bucket ladders; a histogram without one fails `metrics::init`
  (would scrape as a summary)
- `metrics::init` binds `metrics_endpoint` before it installs the recorder, and a bind failure
  fails startup: a recorder with no listener would record samples that nothing drains
