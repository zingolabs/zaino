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

> **Status: initial sync measured; concurrency and serve rate not yet.**
> Section 1 is filled in from a real mainnet run. Sections 2 and 3 are still
> `TBD` — do not quote them.
>
> **These numbers are not `dev`.** They were produced on
> `add_sync_plus_concurrency_tests` at commit `14720e74`, which carries three
> bulk-sync performance changes not yet on `dev` (pipelined batch commits,
> concurrent block fetch, concurrent block assembly). A `dev` measurement would
> be materially slower and is not recorded here.

---

## 1. Initial sync — mainnet, from genesis

`zaino-bench sync`, against a fully synced mainnet zebrad, with Zaino's
finalised-state database removed beforehand. Persistent mode
([`zainod-bench-mainnet.toml`](example_configs/zainod-bench-mainnet.toml)).

| Configuration | Blocks synced | Wall-clock | Mean blocks/s |
|---|---|---|---|
| Direct backend (`backend = 'direct'`) | 3,337,015 | **4h 43m** (16,981s) | **196** |
| RPC backend (`backend = 'rpc'`) | TBD | TBD | TBD |

The RPC row is worth having for contrast: it is what Zaino can do against a
validator it does not share a machine with, and the gap between the rows is the
value of co-location. It is optional — record it only if a second full sync is
worth the wall-clock.

### The mean hides the shape of the run

Sync rate varies by more than two orders of magnitude across the chain, and one
narrow band of heights dominates the wall-clock:

| Region | Heights | Blocks | Wall-clock | Blocks/s | Share of blocks | Share of time |
|---|---|---|---|---|---|---|
| Pre-sandblast | 107,349–1,705,519 | 1,598,170 | 12m 23s | 2,151 | 48% | 4% |
| **Sandblast** | 1,705,519–1,823,494 | 117,975 | **3h 12m** | **10.3** | **3.5%** | **68%** |
| Post-sandblast | 1,823,494–3,444,364 | 1,620,870 | 1h 19m | 342 | 49% | 28% |

**The sandblast heights are 3.5% of the chain and 68% of the sync.** A mean
blocks/s figure quoted without this is close to meaningless: the same node does
2,151 blocks/s before height 1.7M and 10 blocks/s just after it.

The cause is measured, not inferred. A profile through that band puts ~91% of
cycles in BLS12-381 scalar arithmetic — `sqrt_tonelli_shanks`, `Scalar::square`,
`Scalar::invert` — against ~1% in LMDB. That is Jubjub point decompression:
zebra's deserializer resolves `cv` and `ephemeral_key` into curve points for
every Sapling output, and Zaino re-serialises them to store the compact form.
Sandblast-era blocks carry hundreds of outputs each. Both directions are work
Zaino does not need — the bytes are already on disk in compressed form — and
removing rather than parallelising it is an upstream `zebra-chain` change (lazy
`cv` / `ephemeral_key` decompression).

### Caveats on these numbers

Three things about how this run was measured, all of which affect how much
weight the figures carry:

- **The first 107,349 blocks are not in the window.** `zaino.sync.finalized_height`
  is only emitted once the write loop's throttled progress branch first fires,
  so the harness's t0 lands after the node has already synced its first batch.
  Wall-clock from zainod start is roughly 50s longer than the 16,981s recorded.
- **The run ended on the harness's stall timeout, not on completion.** The
  durable tip (`zaino.db.tip_height`) reached the target 3,454,128 — the sync
  finished — but the fetch-pointer gauge the harness watches stopped updating at
  3,444,364, so `--stall-timeout-secs` fired 890s later. That final flat period
  is excluded above; no sync work was outstanding during it. Two harness bugs to
  fix before the next run: completion should be read from `zaino.db.tip_height`
  rather than the fetch pointer, and the `--csv` curve should be written even
  when the run ends in an error (it currently is not, so this run has no curve).
- **The index is 76 GiB, not ~275 GiB.** That is the measured on-disk size at
  the target height, and it contradicts the estimate in
  `zainod-bench-mainnet.toml`. The config comment should be corrected.

Sync curve: not captured for this run (see the second caveat above). When it is,
`--csv` writes `elapsed_secs,
finalized_height, target_height, lag_blocks, node_lag_gauge, db_tip_height,
chain_tip_height, transactions_total, interval_blocks_per_sec`. `lag_blocks` is
derived as `target - finalized`; `node_lag_gauge` is the node's own
`zaino.sync.lag_blocks`, which reports the seam depth rather than the sync lag.

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
| CPU | AMD Ryzen 7 7840U, 16 threads |
| RAM | 61 GiB |
| Disk | **WD_BLACK SN850X NVMe** (LUKS + LVM, mounted at `/`) — zebrad's and Zaino's databases share this volume |
| OS / kernel | Linux 6.1.0-40-amd64 (Debian) |
| `ulimit -n` (client) | 1048576 |
| `net.ipv4.tcp_slow_start_after_idle` | `0` |
| zebrad co-located | yes — Direct backend requires it |

**The disk matters more than anything else in this table.** An earlier attempt at
this run used a USB-attached external drive (reported rotational, queue depth
60), which served random reads at 2,577 IOPS / 0.31 ms against the SN850X's
0.096 ms. On that volume the batch commit alone took 162s per 100k blocks and
consumed 93% of wall-clock; on NVMe it is 38.5s per 100k and ~85%. No number in
this document should be quoted against a non-NVMe volume.

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
| zaino | `14720e74` (branch `add_sync_plus_concurrency_tests`, **not `dev`** — see the status note) |
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

**Disk space.** Measured on this run: the Zaino index is **76 GiB** at height
3,454,128, alongside zebrad's 260 GiB on the same volume. (The ~275 GiB figure in
`zainod-bench-mainnet.toml` is a large overestimate and should be corrected.)
Confirm the free space covers a fresh sync *before* starting one — the sync run
is long enough that losing it partway hurts.

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
