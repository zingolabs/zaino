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
| `/readyz`  | TODO                                   | TODO                         |

- Heartbeat republished by the indexer loop every 100ms

### `/readyz` — TODO

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
| finalised state | mode: `persistent`, `ephemeral(configured)`, `ephemeral(syncing)`, `ephemeral(migrating)`; accumulator rebuild running |
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
