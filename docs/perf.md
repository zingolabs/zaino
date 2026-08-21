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

> **Status: all three sections measured.** Section 2a carries a significant
> methodological caveat — the persistent sweep did not achieve the concurrency
> its connection counts imply — which is documented inline. Read it before
> quoting a supported-connection figure.
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

Both sweeps used `--blocks 200` over the pool `3000000..=3380000`.

### 2a. Persistent finalised state

| Connections | Success | Wall-clock | Mean fetch | p95 fetch | Aggregate blocks/s | Chain breaks | Mean in flight |
|---|---|---|---|---|---|---|---|
| 100 | 100% | 0.25s | 0.040s | 0.051s | 81,316 | 0 | 16 |
| 500 | 100% | 1.08s | 0.039s | 0.050s | 92,821 | 0 | 18 |
| 1000 | 100% | 2.11s | 0.040s | 0.050s | 94,630 | 0 | 19 |
| 2000 | 100% | 4.19s | 0.040s | 0.050s | 95,470 | 0 | 19 |
| 5000 | 100% | 5.40s | 0.039s | 0.050s | 185,266 | 0 | 36 |
| 10000 | 100% | 10.59s | 0.040s | 0.050s | 188,878 | 0 | 38 |

**Supported concurrent connections: not established by this run.** Every round
reports 100% success with flat latency, but the run did not hold its connections
open, so the connection counts in the first column are not concurrency. Read the
next section before quoting any of these figures.

#### Why the persistent sweep is not a concurrency measurement

A finalised read from Zaino's own index takes ~40ms for 200 blocks. The harness
brings connections up across a 2s ramp, so at 10,000 connections a new one starts
every 200µs — and each finishes 40ms later. Connections retire about as fast as
they are created, and the number actually open at any instant is
`connections × mean fetch ÷ wall-clock`: the last column above, which never
exceeds 38 regardless of the nominal count.

Two independent checks agree. The client's `ulimit -n` was **8192** for this run
(the harness warned), which cannot support 10,000 simultaneous sockets — yet all
10,000 succeeded. And wall-clock scales linearly with the connection count
(0.25s → 10.59s for 100 → 10,000), which is the signature of sequential work,
not of concurrent work hitting a ceiling.

What this row set *does* establish: the server absorbed 10,000 successive
short requests at 100% success, with p99 fetch latency flat at 0.052s from 100
through 10,000 and no chain breaks. That is a real result about throughput and
stability. It is not the answer to "how many concurrent connections can it
support", and the honest answer to that question from this run is that the
harness cannot reach the ceiling on the persistent backend: per-connection work
would have to outlast the ramp. Raising `--blocks` until mean fetch exceeds the
ramp (or lowering `--spawn-window-ms`) is the fix, and `ulimit -n` must be raised
past `2 × connections` first.

### 2b. Ephemeral finalised state

| Connections | Success | Wall-clock | Mean fetch | Aggregate blocks/s | Chain breaks | Mean in flight |
|---|---|---|---|---|---|---|
| 100 | 100% | 2.41s | 1.758s | 8,284 | 0 | 73 |
| 500 | 100% | 10.51s | 8.829s | 9,514 | 0 | 420 |
| 1000 | 100% | 20.92s | 18.263s | 9,559 | 0 | 873 |
| 2000 | 100% | 43.56s | 38.151s | 9,182 | 0 | 1,752 |
| 5000 | 100% | 113.73s | 104.477s | 8,793 | 0 | 4,593 |
| 10000 | **2.9%** | 134.61s | 109.848s | 437 | 0 | 8,160 |

**Supported concurrent connections: 5,000.**

This sweep *is* a concurrency measurement, and the contrast with 2a is the
reason. In ephemeral mode a finalised read is a passthrough to the validator and
takes seconds, not milliseconds — far longer than the 2s ramp — so connections
accumulate instead of retiring, and the in-flight count tracks the nominal one
(4,593 of 5,000). The knee is unambiguous: 100% success through 5,000, then
collapse to 2.9% at 10,000, with mean fetch already at 104s by 5,000 against the
30s `service.timeout`.

Aggregate throughput is flat at ~9,000 blocks/s from 100 connections onward,
which says the ceiling is the validator passthrough rather than anything in
Zaino's own request handling.

### Knobs that bound these numbers

Both sweeps were run at **default** settings. These cap concurrency directly, so
a number quoted without them is not reproducible:

| Knob | Where | Value used |
|---|---|---|
| `service.channel_size` | `ZainodConfig` | 32 (default) |
| `service.timeout` | `ZainodConfig` | 30s (default) |
| `storage.cache.capacity` | `ZainodConfig` | 10000 (default) |
| Client `ulimit -n` | test host | **8192 — too low for the 10,000 round** (needs 20,000) |

The `ulimit -n` row is not a footnote: at 8192 the client cannot hold 10,000
sockets open, which is part of why the persistent sweep never reached its nominal
concurrency. Raise it before any rerun.

If a tuned run is also recorded, it goes in its own table naming the knob that
moved — a tuned number presented as the default is misleading.

## 3. Block serve rate

`zaino-bench serve`, one connection, one `GetBlockRange` stream, timed from the
request. The same pass verifies every `prev_hash` link.

| Mode | Range | Blocks | Wall-clock | Blocks/s | Payload MB/s | Errors |
|---|---|---|---|---|---|---|
| Persistent | 3300000..=3454000 | 154,001 | 1.00s | **154,203** | 99.9 | 0 |
| Ephemeral | 3300000..=3454000 | 47,821 (of 154,001) | 120.00s | **399** | 0.2 | timed out |

Both modes verified every `prev_hash` link over the blocks they delivered — the
chain is valid in each case, including the truncated ephemeral stream.

**Persistent serves finalised blocks ~390× faster than ephemeral.** That is the
whole point of the index: in persistent mode the read is answered from Zaino's
own finalised state, in ephemeral mode it is a passthrough to the validator. The
ephemeral run did not finish — it hit the gRPC deadline after 47,821 blocks and
120s, which is itself the finding: a full-range `GetBlockRange` over 154,001
blocks is not serviceable by passthrough within the client's timeout.

Aggregate (multi-connection) throughput is in the sweep tables in section 2; this
section is the single-stream figure. Note the two are not directly comparable —
the sweeps draw from a 380,001-block pool with heavy range overlap at high
connection counts, so cache hit rates differ.

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

**zebrad** — `[state] cache_dir = '/home/idky137/.cache/zebra'`,
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
rm -rf ~/.cache/zaino/*
makers bench sync --csv sync-mainnet.csv
zainod start --config docs/example_configs/zainod-bench-mainnet.toml

# 2 + 3. Against the now-synced instance, persistent mode.
ulimit -n 32768          # must exceed 2 x the largest sweep value
makers bench concurrent \
  --server http://127.0.0.1:8137 \
  --start-height 3000000 --end-height 3380000 \
  --blocks 200 --sweep 100,500,1000,2000,5000,10000
# --end-height pinned, not defaulted to the tip: the two modes report different
# tips, and an unpinned range makes the persistent and ephemeral rows different
# amounts of work.
makers bench serve --server http://127.0.0.1:8137 \
  --start-height 3300000 --end-height 3454000

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
