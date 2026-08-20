# Zaino performance

Three numbers, and the configuration that produced them. The numbers are
meaningless without the configuration, so the two live in one document and are
updated together.

Everything here is produced by `zaino-bench` — see
[`packages/zaino-bench/usage.md`](../packages/zaino-bench/usage.md) for the run
procedure. Every result is measured on the **Direct backend**: Direct reads
Zebra's `ReadStateService` in-process, which is the fastest path Zaino has and
therefore the honest ceiling to quote. It requires zainod and zebrad on the same
host.

The serve and concurrency sections are reported **twice**, under the two
finalised-state modes, because they measure different machinery:

| Mode | `ephemeral_finalised_state` | What answers a finalised read |
|---|---|---|
| Persistent | `false` | Zaino's own finalised-state index |
| Ephemeral | `true` | passthrough to the validator |

Initial sync is a persistent-mode measurement only — in ephemeral mode there is
no index to build.

> **Status: not yet measured.** The harness and configs are in place; the
> mainnet runs have not been done. Every `TBD` below is a value to fill in from
> a real run — do not quote this document until they are filled, and complete
> the [Test configuration](#test-configuration) section in the same pass.

---

## 1. Initial sync — mainnet, from genesis

`zaino-bench sync`, against a fully synced mainnet zebrad, with Zaino's
finalised-state database removed beforehand. Persistent mode
([`zainod-bench-mainnet.toml`](example_configs/zainod-bench-mainnet.toml)).

| Configuration | Blocks synced | Wall-clock | Mean blocks/s |
|---|---|---|---|
| Direct backend (`backend = 'direct'`) | TBD | TBD | TBD |
| RPC backend (`backend = 'rpc'`) | TBD | TBD | TBD |

The RPC row is worth having for contrast: it is what Zaino can do against a
validator it does not share a machine with, and the gap between the rows is the
value of co-location. It is optional — record it only if a second full sync is
worth the wall-clock.

Sync curve: `sync-mainnet.csv` (from `--csv`), columns `elapsed_secs,
finalized_height, target_height, lag_blocks, db_tip_height, chain_tip_height,
transactions_total, interval_blocks_per_sec`.

## 2. Concurrent connections

`zaino-bench concurrent --sweep`, against the synced instance. Each connection
streams 1000 blocks from its own window of the pool.

### 2a. Persistent finalised state

| Connections | Success | Wall-clock | Mean fetch | p95 fetch | Aggregate blocks/s | Chain breaks |
|---|---|---|---|---|---|---|
| 100 | TBD | TBD | TBD | TBD | TBD | TBD |
| 250 | TBD | TBD | TBD | TBD | TBD | TBD |
| 500 | TBD | TBD | TBD | TBD | TBD | TBD |
| 1000 | TBD | TBD | TBD | TBD | TBD | TBD |
| 2000 | TBD | TBD | TBD | TBD | TBD | TBD |

**Supported concurrent connections: TBD** — the largest row still at 100%
success, with no chain breaks.

### 2b. Ephemeral finalised state

| Connections | Success | Wall-clock | Mean fetch | p95 fetch | Aggregate blocks/s | Chain breaks |
|---|---|---|---|---|---|---|
| 100 | TBD | TBD | TBD | TBD | TBD | TBD |
| 250 | TBD | TBD | TBD | TBD | TBD | TBD |
| 500 | TBD | TBD | TBD | TBD | TBD | TBD |
| 1000 | TBD | TBD | TBD | TBD | TBD | TBD |
| 2000 | TBD | TBD | TBD | TBD | TBD | TBD |

**Supported concurrent connections: TBD.**

### Knobs that bound these numbers

Both sweeps were run at **default** settings. These cap concurrency directly, so
a number quoted without them is not reproducible:

| Knob | Where | Value used |
|---|---|---|
| `service.channel_size` | `ZainodConfig` | 32 (default) |
| `service.timeout` | `ZainodConfig` | 30s (default) |
| `storage.cache.capacity` | `ZainodConfig` | 10000 (default) |
| Client `ulimit -n` | test host | TBD |

If a tuned run is also recorded, it goes in its own table naming the knob that
moved — a tuned number presented as the default is misleading.

## 3. Block serve rate

`zaino-bench serve`, one connection, one `GetBlockRange` stream, timed from the
request. The same pass verifies every `prev_hash` link.

| Mode | Range | Blocks | Wall-clock | Blocks/s | Payload MB/s | Errors |
|---|---|---|---|---|---|---|
| Persistent | TBD..=tip | TBD | TBD | TBD | TBD | TBD |
| Ephemeral | TBD..=tip | TBD | TBD | TBD | TBD | TBD |

Aggregate (multi-connection) throughput is the last column of the sweep tables in
section 2; this section is the single-stream figure.

---

## Test configuration

Every field here changes the numbers above.

**Host**

| | |
|---|---|
| CPU | TBD |
| RAM | TBD |
| Disk | `/mnt/framework_ssd` — zebrad's and Zaino's databases share this volume |
| OS / kernel | Linux 6.1.0-40-amd64 (Debian) |
| `ulimit -n` (client) | TBD — raise before the sweep |
| `net.ipv4.tcp_slow_start_after_idle` | TBD — set to `0`; see below |
| zebrad co-located | yes — Direct backend requires it |

`tcp_slow_start_after_idle` defaults to `1`, which resets TCP's congestion
window after an idle period. zebrad warns about it at startup for its own block
fetching, and it applies just as much to the load generator, whose connections
are idle between spawn and fetch. Set it to `0` before measuring and record
which value was in force:

```sh
sudo sysctl -w net.ipv4.tcp_slow_start_after_idle=0
```

**Versions**

| | |
|---|---|
| zaino | TBD (commit) |
| zebrad | 6.3.0, git `f5c5277`, opt-level 3, **`debug checks: true`** |
| Zebra state format | v28 (matches `zebra-state 13.0`; no migration on open) |
| rustc | see `rust-toolchain.toml` |

The zebrad build has `debug_assertions` compiled in, which slows the validator.
It affects the Direct-backend rows least — Zaino reads Zebra's database rather
than asking it questions — but it bounds the optional RPC-backend row and
zebra's ability to hold the tip during a long run. Quote it alongside the
numbers; a validator build is part of the configuration.

**zainod** — [`zainod-bench-mainnet.toml`](example_configs/zainod-bench-mainnet.toml)
and [`zainod-bench-mainnet-ephemeral.toml`](example_configs/zainod-bench-mainnet-ephemeral.toml),
built with `--features prometheus` (required for the `sync` measurement).

**zebrad** — `[state] cache_dir = '/mnt/framework_ssd/.cache/zebra'`,
`[rpc] listen_addr = '127.0.0.1:18232'`, `indexer_listen_addr = '127.0.0.1:18230'`.
Record any other non-default `[state]` or `[sync]` settings.

**Validator must be holding the tip.** Confirm before each run, and record the
connected peer count — a validator running on a handful of peers can fall behind
mid-measurement, which moves the sync target and leaves the concurrency sweep
reading a chain that is still advancing:

```sh
curl -s -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
  http://127.0.0.1:18232 | python3 -m json.tool | grep -E 'blocks|headers|verificationprogress'
curl -s -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getpeerinfo","params":[]}' \
  http://127.0.0.1:18232 | python3 -c 'import json,sys; print("peers:", len(json.load(sys.stdin)["result"]))'
```

A stale `network/mainnet.peers` cache is the usual cause of a low peer count:
delete it and let the DNS seeds repopulate. Note that `[network] cache_dir = true`
means "the default directory", so the peer cache does not follow
`[state] cache_dir` onto the same volume.

**Commands**

```sh
# 0. Host prep, then build. zainod needs the prometheus feature for step 1.
sudo sysctl -w net.ipv4.tcp_slow_start_after_idle=0
cargo build --release -p zainod --features prometheus
cargo build --release -p zaino-bench

# 1. Initial sync, persistent mode. Start the harness first, so t0 is the
#    moment zainod starts. Removing the database is what makes it an *initial*
#    sync rather than a catch-up — check free space first (see below).
rm -rf /mnt/framework_ssd/.cache/zaino/*
makers bench sync --csv sync-mainnet.csv
zainod start --config docs/example_configs/zainod-bench-mainnet.toml

# 2 + 3. Against the now-synced instance, persistent mode.
ulimit -n 8192
makers bench concurrent \
  --server http://127.0.0.1:8137 \
  --start-height 3000000 --end-height 3380000 \
  --blocks 1000 --sweep 100,250,500,1000,2000
makers bench serve --server http://127.0.0.1:8137 --start-height 3300000

# 4. Restart zainod on the ephemeral config, then repeat 2 + 3 unchanged.
zainod start --config docs/example_configs/zainod-bench-mainnet-ephemeral.toml
```

**Disk space.** A full mainnet Zaino index is ~275 GiB, alongside zebrad's
~260 GiB on the same volume. Confirm the free space covers a fresh sync *before*
starting one — an old index backup left in place is enough to run the volume out
partway through, and the sync run is long enough that losing it hurts.

---

## Prior art

The `concurrent` and `serve` tools were ported from the `zaino-admin` binary on
the `hahn/store` branch (`hhanh00/zaino`, commit `14401f07`), which measured a
different indexer — an LMDB block store that branch also introduced — against a
Zaino of that vintage. Those numbers are not comparable to the ones above and are
deliberately not reproduced here: this document measures the `dev` indexer on the
Direct backend. The harness, the sweep, the tail percentiles, the ephemeral /
persistent split, and the metrics-based sync measurement are additions on this
side.
