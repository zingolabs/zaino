# `zainod` — usage

The binary and `zainodlib`. This guide covers the admin listener (`metrics_endpoint`, feature
`prometheus`); configuration and serving are documented in the README.

## The admin listener

One listener, own thread, own current-thread runtime — a probe answered from the saturated serving
runtime measures that runtime's queue, and a timed-out liveness probe gets the pod killed.

| Path       | Answers                                             | Fails (`503`) when           |
| ---------- | --------------------------------------------------- | ---------------------------- |
| `/metrics` | Prometheus exposition: counters, gauges, histograms | never (render panic → `500`) |
| `/livez`   | the indexer loop is still running                   | no heartbeat for 30s         |
| `/health`  | modes & flags as JSON (below)                       | no heartbeat for 30s         |

The indexer loop republishes the heartbeat every 100ms. `/readyz` is not served yet: readiness
arrives with per-component `ComponentStatus` reporting (`zaino-component`).

```json
{"finalised_state_mode":"persistent","mempool_completeness":"complete","accumulator_rebuild_active":false}
```

- `finalised_state_mode`: `persistent`, `ephemeral(configured)`, `ephemeral(syncing)`,
  `ephemeral(migrating)`
- `mempool_completeness`: `complete`, `incomplete(capacity_limited)`,
  `incomplete(pending_metadata)`, `incomplete(source_error)`
- All three `null` until the indexer service exists

## Quantities on `/metrics`, modes on `/health`

A metric is a quantity: a count, a height, a duration, a size. A mode is not. Encoded as a gauge
(`0 none, 1 read-only, 2 full`) a dashboard averages it, `rate()` differentiates it and an alert
thresholds it — all meaningless, none an error. A new mode or flag goes on `/health`.

`ephemeral(syncing)` vs `ephemeral(migrating)` matters operationally: during sync routed writes still
append to the database; during a migration the database is frozen and every routed write lands on the
passthrough. A caller gating on "the real index is serving" waits for `persistent`.

## Metric names

Each emitting crate declares its names and `# HELP` text in its own `metric_names` module: plain
`const`s plus `COUNTERS` / `GAUGES` / `HISTOGRAMS` tables. `zainod` registers them and owns the bucket
ladders; a histogram without a ladder fails `metrics::init` (it would silently scrape as a summary).
