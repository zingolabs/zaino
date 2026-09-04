# `zaino-bench` — measuring a running Zaino

Three questions, three subcommands, all asked from *outside* the node over the
interfaces a real client uses:

| Question | Command |
|---|---|
| How long does it take to sync mainnet? | `zaino-bench sync` |
| How many concurrent connections can it support? | `zaino-bench concurrent --sweep …` |
| How fast can it serve blocks? | `zaino-bench serve`, and the sweep's throughput column |

The harness is a workspace member but **not** a `default-member`: it is an
operator tool, not part of the fast test loop. A bare `cargo nextest run` never builds it. Select it
explicitly:

```sh
makers bench <SUBCOMMAND> [ARGS]         # cargo run --release -p zaino-bench --
cargo nextest run -p zaino-bench         # the harness's own unit tests
```

Always run it **release**. A debug load generator spends its time in its own
code and measures itself rather than the server; `makers bench` enforces this.

## The node under test

Results are only meaningful next to the config that produced them. Two configs
in `docs/example_configs/` are the ones behind `docs/perf.md`, differing in one
line:

| File | `ephemeral_finalised_state` |
|---|---|
| `zainod-bench-mainnet.toml` | `false` — persistent |
| `zainod-bench-mainnet-ephemeral.toml` | `true` — ephemeral |

Three things about them matter:

- **`backend = 'direct'`** — the Direct backend reads Zebra's `ReadStateService`
  in-process, which is the fastest path Zaino has. It requires zainod and zebrad
  on the same host. Measuring `'rpc'` instead answers a different question (what
  Zaino can do against a *remote* validator), so say which one you measured.
  `'state'` is the legacy spelling of `'direct'`; either parses.
- **`ephemeral_finalised_state`** — in persistent mode a finalised read is
  answered from Zaino's own index; in ephemeral mode Zaino keeps no index and
  passes the read through to the validator. Those are different machinery, so
  `concurrent` and `serve` are run and reported under both. `sync` is
  persistent-only: there is no index to build in ephemeral mode, the
  `zaino.sync.*` gauges are never emitted, and the harness would wait forever
  for a metric that never arrives.
- **`metrics_endpoint`** — `sync` reads its progress from there, which means
  zainod must be built with the `prometheus` feature:

  ```sh
  cargo build --release -p zainod --features prometheus
  zainod start --config docs/example_configs/zainod-bench-mainnet.toml
  ```

  Without that feature the endpoint never binds and `sync` waits forever; the
  other two subcommands do not need it.

## `sync` — initial sync time

Samples `zaino.sync.finalized_height` / `target_height` until the finalised
height catches up with the height being synced to, then prints wall-clock time,
blocks synced, and mean blocks/s.

Completion is derived from those two heights, deliberately, rather than read
from the node's own `zaino.sync.has_reached_tip` and `zaino.sync.lag_blocks`
gauges. Neither means what its name suggests:

- `has_reached_tip` is set when the sync loop's iteration returns `Ok`, and
  `sync_to_height` returns `Ok` as soon as it has *spawned* the background sync
  (it is single-flight, so a poll landing on an in-flight sync is also an `Ok`
  no-op). It goes to 1 seconds after start-up and stays there — it means "the
  sync loop is healthy", not "the index is at the tip".
- `lag_blocks` is `chain_tip - finalized_height_floor(chain_tip)`: the
  non-finalised seam depth, a constant, not the distance left to sync.

Both are still recorded — `has_reached_tip` in the summary, `lag_blocks` in the
CSV's `node_lag_gauge` column — so a run carries the node's own readings next to
the derived ones.

**Start it before zainod.** It waits for the `zaino.sync.finalized_height`
gauge to appear — not merely for `/metrics` to answer, since zainod binds the
exporter well before the write loop first sets that gauge — so t0 is the
moment the node starts rather than the moment you reached a second terminal:

```sh
# terminal 1
makers bench sync --csv sync-mainnet.csv
# terminal 2, once the harness says it is waiting
zainod start --config docs/example_configs/zainod-bench-mainnet.toml
```

To measure a *fresh* sync, empty `storage.database.path` first — an existing
finalised-state db means you are measuring a catch-up, not an initial sync.
Check free space before you do: a full mainnet index measures ~76 GiB and shares
a volume with zebrad's ~260 GiB. Use an NVMe volume — a USB-attached disk makes
the batch commit the bottleneck and roughly halves the sync rate.

`--csv` writes every sample (elapsed, heights, lag, transactions, interval rate)
for graphing the curve. **Known bug:** the file is only written when the run ends
normally, so a run that ends on `--stall-timeout-secs` discards its curve. `--until-height N` bounds the run to a fixed span instead
of waiting for the tip. `--stall-timeout-secs` fails the run, non-zero, if the
finalised height stops advancing — so an overnight run does not silently hang.

## `concurrent` — connection ceiling and aggregate throughput

Spawns N clients, each streaming `--blocks` blocks from its own window of
`--start-height..=--end-height`, and reports success/failure counts, connect and
fetch latency (min/mean/max **and** p50/p95/p99), chain breaks, and aggregate
plus per-connection blocks/s.

```sh
ulimit -n 32768          # must exceed 2 x the largest sweep value
makers bench concurrent \
  --server http://127.0.0.1:8137 \
  --start-height 3000000 --end-height 3380000 \
  --blocks 200 --sweep 100,500,1000,2000,5000,10000
```

Use `--sweep`, not a single `--connections` value. The answer to "how many
connections can you support" is the knee — the largest round still at 100%
success — and one point sample cannot locate it. The sweep prints a comparison
table with exactly that reading in mind.

Two things that will otherwise give you the wrong number:

- **File descriptors.** Each connection needs a socket. The harness reads
  `/proc/self/limits` and warns when the soft limit is under roughly
  `connections × 2`; heed it (`ulimit -n 32768` for a 10,000 round) or the
  ceiling you measure is this client's, not the server's.
- **Per-connection work must outlast the ramp, or the round measures nothing.**
  Connections are brought up over `--spawn-window-ms` (default 2000). If each
  finishes faster than the ramp creates the next, they retire as fast as they
  arrive and the number actually open never approaches the nominal count — the
  round becomes a throughput test wearing a concurrency test's label. A
  persistent-mode finalised read of 200 blocks takes ~40ms, so a 10,000-connection
  round held only ~38 open at once (see `docs/perf.md` §2a). Sanity-check every
  round with `connections × mean fetch ÷ wall-clock`; if that is far below the
  nominal count, raise `--blocks` until mean fetch exceeds the ramp.
- **Server-side bounds.** `service.channel_size` (default 32) and
  `service.timeout` (default 30s) in `ZainodConfig` cap concurrency directly.
  Run at defaults first. If you tune them, report both numbers and name the knob
  you moved — an untuned number and a tuned one are both interesting, and a
  tuned number presented as the default is misleading.

Partial failure in a sweep is a *result*, not an error, so the run still exits
zero; a round set where no connection anywhere succeeded exits non-zero.

## `serve` — single-stream serve rate

One connection, one large `GetBlockRange`, timed from the request (not the
connect — connect cost is the load test's concern). Reports blocks/s, approximate
payload MB/s, and verifies every `prev_hash` link as blocks arrive, because a
fast answer that does not link up is not an answer.

```sh
makers bench serve --server http://127.0.0.1:8137 \
  --start-height 3300000 --end-height 3454000
```

Pin `--end-height` rather than letting it default to the tip when comparing the
two finalised-state modes: they report different tips, so an unpinned range makes
the two runs different amounts of work.

Run it — and the sweep above — once per finalised-state mode: restart zainod on
the other config and repeat the same commands unchanged.

`--end-height` defaults to the server's tip. The run exits non-zero if the chain
does not link. A mid-stream gRPC failure is reported and summarised rather than
swallowed — under load that failure *is* the finding.

## Where the results go

`docs/perf.md` holds the published numbers, and alongside them the machine spec,
zebrad and zainod versions and configs, `ulimit -n`, and the exact commands.
Numbers without that section are not reproducible; update them together.
