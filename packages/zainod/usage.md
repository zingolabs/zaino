# `zainod` — usage

The daemon binary: its offline disk check, failure policy, logging and admin listener.
Configuration and running the daemon are in [`docs/running.md`](../../docs/running.md).

## The admin listener

With `[metrics] listen_address` set, zainod serves an admin listener on
its own thread and current-thread runtime. A probe answered from a saturated serving runtime
would measure that runtime's queue, and a timed-out liveness probe gets the pod killed.

| Path       | Answers                                | Fails when                     |
| ---------- | -------------------------------------- | ------------------------------ |
| `/metrics` | Prometheus exposition, process metrics | render panic (`500`)           |
| `/livez`   | the serving runtime still schedules    | no heartbeat for 30s (`503`)   |
| `/readyz`  | `{"ready", "reasons"}`                 | any reason below (`503`)       |
| `/statusz` | one JSON snapshot (below)              | never (readiness in the body)  |

`/readyz`, `/statusz` and the `/metrics` gauges each read **one** load of the global snapshot
(`zaino-snapshot`): none of them can mix two moments.

`/readyz` reasons: `draining` (a shutdown signal arrived; listed first, see
[systemd](#systemd)), `starting` (indexer not booted), `heartbeat_stale`, then the snapshot's
own, in order: `headers_syncing` (no verified header chain tip yet), `tip_not_held` (no trusted
validator holds the verified tip), `syncing` (the served tip is not at the verified best: on
once it is, off once it leaves the best chain or trails it by more than `finalised_depth`;
requests are never refused for it).

`/statusz` = zainod's process fields beside the snapshot's report (`zaino_snapshot::Report`,
flattened; shape in [`docs/running.md`](../../docs/running.md#status)): `version`, `network`,
`uptime_s`, `ready`, `reasons` (as `/readyz`), `grpc.sent_bytes` (response body bytes served,
all methods), `disk` (per enabled index: `size_bytes` + `tables` = bytes per subdirectory, from
the progress task's last walk, so absent until the first, 30 s after boot, then every 2
minutes), then `seq`, `tips` (`best`, `final`, `served` as `{height, hash}`, `held_by` of
`configured`, `synced`), `unready`, `handed` (the last block handed to the indexes: sync progress
between their commits), `indexes` (every index the NFS can fold: `name`, `enabled` = configured,
`durable` = its committed height, `null` until the NFS reports it; listed from startup, snapshot
bootstrap included), `validators` (each configured one:
`address`, `state` / `latency_ms` / `failures` = the traffic balancer's, `agreement`, `height`,
`stale_blocks`, `observed_s_ago`, `streaming` (push streams up), `release`: build, user agent,
protocol and `end_of_service` as `{"status": "at", height, estimated_unix, blocks_left}` /
`{"status": "not_enforced"}` / `{"status": "unknown"}` (zebrad < 6.3), and the p2p `peers` its
`getpeerinfo` reports), `alarms` (`partitioned`, `eclipsed`, `finality_paused`, `stale`, `ending`
= releases halting within a week), `mempool` (`transactions`, `verified`, `ours_unverified`,
`fully_spread`, `trusted_readers`), `forks` (each side branch: `from`, `tip`, `cumulative_work`
as a decimal string, `folded`). Bulk sync moves `durable` per batch; at the tip, each final block
moves it by one. Traffic data: keep the listener private.

`/metrics` sets every state gauge from the snapshot first (`zaino_best_tip`,
`zaino_fetch_height`, `zaino_index_synced{index}`, the `zaino_chainview_*` gauges:
[`zaino-snapshot`](../zaino-snapshot/usage.md#reporting)), then renders; events
(`zaino_reorgs_total`, `zaino_fetch_*_total`, histograms) are counted where they happen
([`zaino-nfs`](../zaino-nfs/usage.md#observability)). Each index writer reports its own progress
as it applies blocks ([`zaino-sync`](../zaino-sync/usage.md)): `zaino_index_applied_height`,
`zaino_index_applied_blocks_total`, `zaino_index_applied_rows_total`, per commit
`zaino_index_finalized_height`, and per run `zaino_index_run_seconds` / `zaino_index_write_seconds`
(the store's share of it; run − write = the fold + waits) (all `{index}`). One progress task logs the sync summary and each
index's `Syncing` line (applied, durable, size) every 30 s.

The same task samples the disk every 30 s (a metric whose source is unreadable, e.g. no cgroup v2
PSI, is left out rather than reported as zero):

| Metric | Labels | Answers |
|---|---|---|
| `zaino_disk_committed_bytes` | `index`, `table` | committed data per table |
| `zaino_disk_cached_bytes` | `index`, `table` | how much of it sits in page cache (`mincore`; the rest = a disk read on first touch) |
| `zaino_disk_bytes` | `index` | the index directory on disk, preallocation and unlanded merges included (the 2-minute walk) |
| `zaino_disk_free_bytes`, `zaino_disk_capacity_bytes` | `index` | room left on the index's filesystem |
| `zaino_disk_read_bytes_total`, `zaino_disk_written_bytes_total` | | block-device bytes zainod moved (`/proc/self/io`) |
| `zaino_io_stall_seconds_total` | `kind` = `some` / `full` | time zainod's own cgroup had tasks stalled on I/O |

Each durable commit records `zaino_store_commit_seconds` and `zaino_store_commit_bytes_total`
(`{index}`, [`zaino-persistence`](../zaino-persistence/usage.md)).

- A supervised task on the serving runtime republishes the heartbeat every 100ms.
- The listener binds before the recorder installs, so a bind failure fails startup.
- At most 32 admin connections are served at once; the rest wait in the accept backlog.
- A non-private `listen_address` logs a warning at startup, because `/metrics` is
  unauthenticated.
- The build gauge is `zaino_build_info`, with the version as a label.

## systemd

Run under `Type=notify` and zainod reports its own startup and shutdown to systemd. Without
`NOTIFY_SOCKET` (any other service manager, a container, a shell) nothing is sent.

```ini
[Service]
Type=notify
ExecStart=/usr/bin/zainod start --config /etc/zaino/zainod.toml
Restart=always
# Budget for a start that is not moving (opening indexes, reaching the validator)
TimeoutStartSec=15min
```

| Message | When |
|---|---|
| `READY=1` | the first time `/readyz` passes: serving at the verified tip |
| `EXTEND_TIMEOUT_USEC` (5 min) | each 10 s check before that which saw progress: snapshot bytes, the fetch height, the served height or an index height moved |
| `STATUS=` | the readiness reasons (`ready` once ready), whenever they change |
| `STOPPING=1` + `EXTEND_TIMEOUT_USEC` | on the shutdown signal, covering `[grpc.shutdown]`'s delay and timeout plus 30 s for the index flush |

- `systemctl start` / `restart` returns only once zainod serves at the tip, so a deploy
  that restarts it (`nixos-rebuild switch`, deploy-rs) sees an upgrade that cannot catch up as
  a failed unit.
- A fresh sync or snapshot bootstrap keeps extending its start for as long as it moves, so
  `TimeoutStartSec` bounds only a stalled start (5 minutes without progress, once past it).
  Units ordered after zainod, and `multi-user.target` at boot, wait for that start; use
  `systemctl start --no-block` to return at once.
- One extension is at most 71 minutes (a 32-bit µs count): a longer shutdown drain still needs
  that much `TimeoutStopSec`.
- `NOTIFY_SOCKET` must be a filesystem path (systemd's default), not an abstract `@` socket.

With `[grpc.shutdown]` on, a `systemctl stop` or `restart` drains: `/readyz` reports
`draining` for `delay_secs` while gRPC keeps serving, then the listener closes and open
connections get `timeout_secs` (see [Stopping and restarts](../../docs/running.md#stopping-and-restarts)).
A crash restart skips the drain (no signal arrives).

## Index snapshot

Built with the `snapshot` feature (`cargo build --features snapshot`, Nix
`zainod.override { features = [ "snapshot" ]; }`, Docker `--build-arg CARGO_FEATURES=snapshot`)
and `aria2c` on `PATH`, zainod fills empty indexes from a published snapshot before it boots:

```toml
[snapshot]
manifest = "https://snapshots.zingolabs.dev/zaino-snapshot-1.1.0/manifest.json"
connections = 8   # total across every archive (aria2c --split), at most 16
```

The manifest lists one archive per index, all from one seed at one `height` (each `archive`
resolves against the manifest URL; `installed_bytes` = its unpacked size):

```json
{"network": "mainnet", "height": 3501802,
 "indexes": {
   "compact_block": {"archive": "compact_block.tar.zst", "sha256": "…",
                     "archive_bytes": 31000000000, "installed_bytes": 74000000000},
   "value_balance": {"archive": "value_balance.tar.zst", "sha256": "…", …},
   "block_hash": {…}, "tree_state": {…}, "transparent_address": {…}}}
```

Each archive is a zstd tar of that index directory's contents (no top-level directory). Keys
are the index names: `compact_block`, `value_balance`, `block_hash`, `tree_state`,
`transparent_address` (`value_balance` installs beside the compact-block index, which it runs
with; `index.compact_block` may be disabled, and then neither is installed).

- Only an enabled index whose directory is missing or empty is filled, from its own archive
  (only those are downloaded); one holding data is never touched, and with none empty the
  snapshot is skipped (no download). An empty index the manifest has no archive for stops startup.
- Before any download, free space is checked per filesystem: each archive plus its unpacked size
  (less what a previous attempt already staged). Too little stops startup and names the
  directory, the bytes needed and the bytes available.
- aria2c downloads over its JSON-RPC (loopback, random secret): every archive queued, one at a
  time over `connections`, retried without limit on transient errors, resumed after a restart,
  each checked against its `sha256` by aria2c. It stops itself if zainod dies.
- Each archive is unpacked as soon as it lands (while the next downloads) into
  `.<index>.snapshot` beside its index, then renamed into place and deleted. Index paths may sit
  on different filesystems.
- A restart resumes each archive at the step it stopped in (download, unpack, install).
- A manifest for another network, a hash mismatch or a permanent download error stops startup
  (a failed archive's staging is deleted).

While it runs, `/readyz` answers `snapshot_downloading` / `snapshot_verifying` /
`snapshot_unpacking`, and `/statusz` carries the progress beside the configured `indexes`:

```json
{"ready": false, "reasons": ["snapshot_downloading"],
 "indexes": [{"name": "value_balance", "enabled": true, "durable": null}, …],
 "snapshot": {"phase": "downloading", "source": "snapshots.zingolabs.dev", "height": 3501802,
              "indexes": 5, "done": 27000000000, "total": 44500000000, "rate": 103000000}}
```

`indexes` = archives fetched, `total` = their cumulative bytes. `phase` = the slowest archive's
(each moves downloading → verifying → unpacking); `done` = bytes through it, archives already
past it counted whole. `rate` bytes/s (downloading only). The `Snapshot` component logs progress
every 30 s:

```text
INFO  Snapshot:  Downloading index snapshot  from=snapshots.zingolabs.dev to=3,501,802 indexes=5 done=27.0GB total=44.5GB rate=103MB/s eta=2m49s
INFO  Snapshot:  Index snapshot installed    indexes=5 height=3,501,802 elapsed=7m12s
```

## `zainod verify`

```
zainod verify --config /etc/zaino/zainod.toml
```

Loads the same config the daemon runs with and, for every **enabled** index,
reads every file its `MANIFEST` seals against that file's page checksums
([`docs/design/durability.md`](../../docs/design/durability.md) §3). The same
check for every index: the bytes on disk are the bytes that were sealed. What
they mean was settled while they were built, so nothing here decodes or
re-derives them. A disabled index is skipped and reported as `null`.

Safe beside a live daemon: plain sequential reads, no mmap, no lock, nothing
created or written. Bytes past a file's seal (a live writer's next batch, a
crash's leftovers) are counted as `orphaned_bytes`, never as corruption. A merge
can retire segments between reading the manifest and scrubbing them; a missing
file the manifest no longer lists is retired, not lost, and that index is
scrubbed again against the newer manifest.

### Output and exit status

The full report goes to stdout as JSON, a one-line-per-index summary to stderr
(logging is not initialised for this subcommand). Exit 0 when every sealed file
is present with every page intact, 1 when any file is lost or any page corrupt,
2 when the config or a manifest could not be read (a manifest built for another
network or format included).

```json
{
  "clean": true,
  "compact_block": {
    "heights": 4,
    "files": [
      { "path": "blocks.dat", "committed_bytes": 1234, "orphaned_bytes": 0, "lost": false, "bad_sums": false, "bad_pages": [] },
      { "path": "offsets.idx", … }
    ]
  },
  "block_hash": { "heights": 4, "files": [ … by_hash/*.seg ] },
  "tree_state": { "heights": 4, "files": [ … heights.idx, then per pool l00.dat … l31.dat, subtrees.dat ] },
  "transparent_address": { "heights": 4, "files": [ … receives/*.seg, spent/*.seg ] }
}
```

`bad_pages` = 4 KiB page indexes whose CRC disagrees; `bad_sums` = the `.crc` file no longer matches the digest the manifest committed. A corrupt or lost file
means delete that index's directory and resync it.

## Failure policy

zainod dies rather than serve from a state it cannot vouch for
([`docs/design/durability.md`](../../docs/design/durability.md) §6):

- any panic aborts the process, including a page checksum mismatch on the read
  that first touches a corrupt page
- index writers are infallible and panic on any failure. A failed commit names
  the index and its directory: `<index> index commit failed: disk <dir> full`,
  or `… failed at <dir>: <error>`
  ([zaino-sync: Failure](../zaino-sync/usage.md#failure-panic-never-err))
- any other task that ends first (the NFS, chainview poller, gRPC), cleanly or
  not, cancels the rest and `zainod start` exits 1 (`IndexerError::TaskEnded` or
  the task's own error, e.g. `NfsError::Diverged` = an index committed off the
  verified chain: resync it)
- there is no in-process restart, so run it under a service manager that
  restarts it
- an index directory built for another network or format, shorter than its
  seals, or whose tail page fails its checksum refuses to open: resync it
- `sync.finalised_depth` defaults to Zebra's reorg bound (1000,
  `MAX_BLOCK_REORG_HEIGHT`); below it is a config error on mainnet and testnet,
  allowed on regtest only

## Logging

`zainodlib::logging::init()` installs the global `tracing` subscriber and the
panic hook; `zainod start` calls it, `zainod verify` does not (stdout carries
only the JSON report). Everything is read from the environment:

| Variable | Effect |
|---|---|
| `RUST_LOG` | Standard tracing filter. Unset, only zaino crates log at `info`; `RUST_LOG=info` includes every crate, or filter explicitly (`RUST_LOG=zaino=info,zaino_persistence=debug,tonic=warn`). |
| `ZAINOLOG_FORMAT` | `terminal` (default, one line per event) or `json` (machine-parseable, spans included). |
| `ZAINOLOG_COLOR` | `true`/`false` to force ANSI colour, `auto` (default) to colour only when stdout is a terminal. |
| `ZAINOLOG_LOCATION` | `true` adds each event's source as `at=file:line`. Off by default. |

An unrecognised value for any `ZAINOLOG_*` variable, or a malformed `RUST_LOG`,
stops startup with an error that names the variable. There is no fallback
format.

The `terminal` format follows go-ethereum's layout, with a component column:

```text
WARN  [09-28|17:29:15.175] ChainView:           Validator catching up          endpoint=zebrad:18232 height=3,434,171 behind=65,512 hash=00000000…1a76bf89
INFO  [09-28|17:29:42.659] ZainoNFS:            Syncing blocks                 height=3,503,023 target=3,506,659 bps=10 eta=5m49s
INFO  [09-28|17:29:42.659] CompactBlockIdx:     Syncing                        durable=3,501,802 size=31.0GB
INFO  [09-28|17:29:42.659] BlockHashIdx:        Syncing                        durable=3,501,802  size=134MB
```

- Each line has a 5-character level, a UTC `MM-DD|HH:MM:SS.mmm` timestamp, the
  component that logged it, and the message, padded to 30 columns when fields
  follow, so repeated lines align. Index `durable` heights and `size` are padded
  into columns (between fields, never inside a value).
- Components: `Zainod` (lifecycle), `Metrics`, `Traffic` (the balancer's driver: every
  validator's poll), `ChainView` (polls folded, headers, mempool, submission), `ZainoNFS`
  (block fetch, folds, the served tip, reorgs), `Grpc`, and one per index
  (`CompactBlockIdx`, `ValueBalanceIdx`, `BlockHashIdx`,
  `TreeStateIdx`, `TransparentAddrIdx`), which also owns that index's
  commits and compactions. In `json` the component is the `component` field of
  the event's enclosing span.
- Fields are `key=value` logfmt, parseable by Loki and `hl`. Integers from
  1,000 up, heights included, are grouped with commas (`json` keeps them plain
  numbers). A 64-hex hash shows its first and last 8 digits (`json` keeps it
  whole). A value containing a space or `=` is quoted, and an error field
  carries its whole source chain. A path in a message longer than 48 columns
  keeps its tail (`…/zaino/indexes/compact_block`).
- Fields from enclosing spans follow the event's own fields.

What an operator sees at `info`:

| Component | Event | Level | When |
|---|---|---|---|
| `ChainView` | `Chain view configured` | info | At startup (`validators`, `headers_final` = the header chain's final height resumed from disk). |
| `ChainView` | `Polling validator` | info | A validator's first listing (`endpoint`, `listed` = its mempool size). |
| `ChainView` | `Headers verified and final` | info | Header sync finalized past another 100,000 heights (`height`). |
| `ChainView` | `Header fetch failed` | warn | Header sync skips that validator this round and retries in 5 s (`endpoint`, the failure, or no member to ask: benched, down). |
| `ChainView` / `ZainoNFS` | `Misanswer, member benched` | warn | A validator served a header that fails a rule or does not decode (header sync), or a block its verified header does not commit to (NFS): `member`, `class`, `why`. Nothing but its poll reaches it for 60 s (doubling, 1 h at most); the block is asked of another. |
| `ChainView` | `Validator poll failed` / `Validator down, holds no tip` / `Validator back` | warn / warn / info | A failed poll (the traffic balancer's 0.5 → 30 s ladder); 10 in a row (its chain and sightings retracted, probed every 30 s); its first answer after. |
| `ChainView` | `Transaction not accepted` | warn | A submission ended with no acceptance (`txid`, `attempts`, the rejection or failure). |
| `ChainView` | `Validator catching up` | warn | Every 60 s while a validator's mempool is off below the network tip (`endpoint`, its `height`, `behind` its own network estimate, `hash`). |
| `ChainView` | `Validator caught up` | info | The mempool answers again. |
| `Snapshot` | `Validator tip stale against its own clock (stalled or eclipsed)` / `Validator tip fresh again` | warn / info | A live validator's tip falls ≥ 24 blocks behind its own `estimatedheight`, then recovers (`endpoint`, `tip`, `behind`). |
| `Snapshot` | `Two live validators share no outbound peer (possible partition)` / `…share outbound peers again` | warn / info | Edge of the partition check over `getpeerinfo`. |
| `Snapshot` | `Live validators reach few distinct outbound peers (possible eclipse)` / `…enough distinct outbound peers again` | warn / info | Edge of the eclipse check (1 to 2 distinct outbound peers across live validators; none at all raises nothing). |
| `Snapshot` | `Finality paused: no trusted validator holds the boundary` / `Finality resumed` | warn / info | A boundary block is `finalised_depth` deep but no trusted validator holds it, then one does (`best`). |
| `ChainView` | `Validator peer list read failed, last kept` / `Validator release read failed, last kept` | warn | A `getpeerinfo` or `getinfo` / `getdeprecationinfo` failure (telemetry only: the poll carries on). |
| `Snapshot` | `Validator release reaches end of service soon (it halts there): upgrade it` / `Validator release upgraded` | warn / info | A validator's release halts within a week of its tip (`endpoint`, `build`, `left` blocks), then a newer release clears it. |
| `ChainView` | `Push stream up` / `Push stream ended, polling meanwhile` | info / warn | A validator's indexer push streams (`indexer`) open, or end (lag, restart, network); it reconnects on a 500 ms → 30 s ladder and polls every second meanwhile. |
| `ChainView` | `Push stream unavailable, polling` | info | Once, when the configured `indexer_address` refuses (retries then stay at debug). |
| `Grpc` / `Metrics` | `Listening` | info | At startup (`endpoint`; gRPC adds `network`). |
| `Grpc` | `Serving` | info / warn | Every 60 s while anything is served or held: `rps`, `p99` time to first message, `out` bytes/s (3 significant figures), `conns` held; warn with `failed`, `refused`, `at_capacity`, `slow`, `slowest`, `stalled`, `conns_refused` when any is non-zero ([zaino-grpc: Serving log](../zaino-grpc/usage.md#serving-log)). |
| `Grpc` | `High load` | warn | In place of `Serving` while any cap (`streams`, `subs`, `conns`) is past 25% held: those caps as `used/max`. |
| `Grpc` | `Request failed` | error | A request's first server fault in a minute (`method`, `code`, `error`); later ones only counted. |
| `Grpc` | `Request unavailable` | warn | The first refusal in a minute other than a full admission pool (nothing served yet, validator unreachable: `method`, `error`). |
| `ZainoNFS` | `Syncing blocks` | info | Every 30 s (the progress task) while the blocks handed to the indexes trail the verified best (`height` handed, `target` = the best, `bps`, `eta`). |
| `ZainoNFS` | `Block fetch stalled` | warn | A whole 30 s interval behind the best handed over no block. |
| `ZainoNFS` | `Chain tip advanced` | info | Each published served tip at the verified best (`height`, `hash`, block `age`, `finalized` = the header chain's final tip). |
| `ZainoNFS` | `Chain reorg detected` | warn | The served tip left the best chain (`from` = its height, `to` = the new served tip). |
| `Snapshot` | `Serving the verified tip` / `Behind the verified tip, syncing` | info | The `synced` judgement flips between two publishes (`height` = the served tip). |
| index | `Syncing` | info | Every 30 s while not synced (the progress task): `durable` (committed, from the snapshot), `size` (bytes on disk, 3 significant figures, decimal units). |
| index | `Index size unreadable` | warn | A directory walk failed (`error`); `size` is left out until the next one succeeds. |
| index | `Commit waited on compaction` | warn | A commit blocked on a merge that fell two windows behind. |

Per-commit lines (`Committed batch`) and every LSM merge (`Compacting segments`,
`Compacted segments`) are at `debug`.

The panic hook logs each panic as a structured `error` event at its origin —
thread, location, and message — so a panic on a worker thread that would
otherwise surface only as a distant `JoinError` is logged through the same sink
as every other error. The `panic` target is forced on so no filter drops it.
The hook then chains to `main`'s, which prints the panic and aborts the process
(see [Failure policy](#failure-policy)).
