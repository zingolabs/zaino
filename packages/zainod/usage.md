# `zainod` — usage

The binary and `zainodlib`. This guide covers the admin listener (`metrics_endpoint`, feature
`prometheus`); configuration and serving are documented in the README.

## The admin listener

One listener, own thread, own current-thread runtime — a probe answered from the saturated serving
runtime measures that runtime's queue, and a timed-out liveness probe gets the pod killed.

| Path       | Answers                                                      | Fails (`503`) when                        |
| ---------- | ------------------------------------------------------------ | ----------------------------------------- |
| `/metrics` | Prometheus exposition: counters, gauges, histograms          | never (render panics → `500`)             |
| `/livez`   | process is alive and still reporting                         | unrecoverable component, or stale status  |
| `/readyz`  | index is usable                                              | syncing, or stale status                  |
| `/health`  | `{"status": …, "finalised_state_mode": …}` as JSON           | stale status                              |

Status and mode are republished by the indexer loop every 100ms; "stale" is 30s without one.

## Quantities on `/metrics`, modes on `/health`

A metric is a quantity: a count, a height, a duration, a size. A mode is not. Encoding one as a gauge
(`0 none, 1 read-only, 2 full`) makes a dashboard average it, a `rate()` differentiate it, and an alert
threshold it — all of them meaningless, none of them an error.

- Which backend serves finalised reads (`FinalisedStateMode`) is a mode → `/health` only:
  `persistent`, `ephemeral(configured)`, `ephemeral(syncing)`, `ephemeral(migrating)`
- `StatusType` is a mode too → `/health` (and the probes, which reduce it to pass/fail)
- A new mode goes on `/health`. It does not get a gauge "for dashboards"

`ephemeral(syncing)` vs `ephemeral(migrating)` matters operationally: during sync routed writes still
append to the database; during a migration the database is frozen and every routed write lands on the
passthrough. A caller gating on "the real index is serving" waits for `persistent`, not for `Ready` —
a passthrough reports `Ready` exactly like a synced database.
