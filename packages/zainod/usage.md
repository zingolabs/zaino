# `zainod` — usage

The daemon binary: its validator client, admin listener and states, operator hold, systemd, index snapshot,
offline disk check, failure policy and logging. Configuration, running the daemon and network
exposure are in [`docs/running.md`](../../docs/running.md).

## Validator client

`validator_client` picks one client type for every `[[trusted_validators]]` entry:

- `legacy_rpc` (default): JSON-RPC for everything, zebrad or zcashd.
- `grpc`: blocks over zebrad's indexer gRPC (`GetBlock`, `GetBlockRange` at each entry's
  `indexer_address`), everything else over JSON-RPC. `GetBlockRange` exists only in Zaino's
  zebra fork (`elicbarbieri/zebra` `perf/indexer-block-range`). An entry without
  `indexer_address` fails boot before anything binds. Client detail:
  [zaino-source](../zaino-source/usage.md).

## The admin listener

With `[metrics] listen_address` set, zainod serves an admin listener on
its own thread and current-thread runtime. A probe answered from a saturated serving runtime
would measure that runtime's queue, and a timed-out liveness probe gets the pod killed. Who
may reach it: [running.md: Network exposure](../../docs/running.md#network-exposure).

| Path       | Answers                                | Fails when                     |
| ---------- | -------------------------------------- | ------------------------------ |
| `/metrics` | Prometheus exposition, process metrics | render panic (`500`)           |
| `/livez`   | the serving runtime still schedules    | no heartbeat for 30s (`503`)   |
| `/readyz`  | `{"state", "detail"}`                  | `state` other than `serving` or `degraded` (`503`) |

`/readyz`, the public [`/status`](#public-health-on-the-grpc-listener) and the `/metrics` gauges
each read **one** `GrpcSnapshot` load (`zaino-grpc-snapshot`): none of them can mix two
moments.

`state` is one of six, and the first rule that matches wins:

| `state`        | When                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------- |
| `held`         | an operator ran `zainod hold` (see [Operator hold](#operator-hold))                      |
| `provisioning` | not booted yet: an index-snapshot bootstrap, or plain startup                            |
| `down`         | boot waiting on its first validator answer, the serving runtime stalled (no heartbeat for 30 s), overloaded (below), the best block more than one past the last a trusted validator vouched for while not `degraded`, or every trusted validator down under a stale best (`[p2p]` off: the tip frozen) |
| `degraded`     | the best block more than one past the last vouched, but synced to it while fresh (its block time within 24 target spacings of the clock: p2p peers carried the tip) |
| `syncing`      | booted, but the served tip is not the best yet, or a validator's mempool is off and none is on (zebrad's, for minutes after a catch-up) |
| `serving`      | otherwise                                                                                |

- `ready` is true exactly when `state` is `serving` or `degraded`, and only then does `/readyz`
  answer `200`. A degraded node keeps serving so a load balancer keeps routing to it during an
  outage of every trusted validator; alert on the state, not on `/readyz`.
- While degraded, finality and the mempool run on the peers' word:
  [architecture: Finality](../../docs/architecture.md#finality),
  [The mempool view](../../docs/architecture.md#5-the-mempool-view).
- `syncing` follows `synced`
  ([architecture §8](../../docs/architecture.md#8-serving-one-grpcsnapshot)). Requests are never
  refused for it.
- `detail` is a short sentence saying why, set only when `down` or `degraded`: `No trusted
  validator reachable` (boot), `No trusted validator vouches for the tip`, `Runtime stalled` or
  `Overloaded` (a degraded node only ever shows the second). One block past the last vouch is the
  race a validator's next poll closes (a peer relayed the block first), not a reason. When `held`
  it names the drain: `GOAWAY pending` or `GOAWAY sent`.
- `Overloaded` is a backstop for a node persistently over capacity: admission already sheds
  bursts within seconds, and this only takes the node out of a load balancer. It follows the gRPC
  work pool's `overloaded` flag (thresholds in
  [zaino-grpc: serve stack](../zaino-grpc/usage.md#serve-stack)).
- `progress` is set only while `provisioning` or `syncing`, as `{done, total, unit, rate}`:
  - A bootstrap reports `unit` `byte`, with `rate` in bytes/s while downloading.
  - Syncing reports `unit` `block`:
    - `done` is the served height, or the handed one before anything is served.
    - `total` is the verified best height, or a catching-up validator's network estimate when
      that is higher.
    - `rate` is blocks per second over the last ~60 s of the phase still outstanding: finalized
      blocks handed to the indexes, then the served height climbing through the non-finalized
      window.
  - `progress` is `null` before the first verified header.
- `notes` holds one sentence per raised alarm
  ([architecture §11](../../docs/architecture.md#11-telemetry)), or `[]` when none.
- `progress` and `notes` appear on the public `/status`; `/readyz` carries `state` and `detail`
  only.

## Operator hold

`zainod hold [--config FILE]` takes a node out of rotation until `zainod resume [--config FILE]`.
It persists across restarts, deploys and reboots. It is also the only drain. A shutdown signal
closes the gRPC listener at once, so a planned stop is `zainod hold`, then `systemctl stop`.

- `hold` writes the marker `<[serve] state_dir>/held` (default `<data dir>/zaino/state`),
  synced to disk with its directory. `resume` removes it. There is no network endpoint, so
  holding a node requires write access to its state directory.
- The running zainod checks the marker every second, from process start (before the
  index-snapshot bootstrap and before the gRPC listener binds). Held:
  - `state` becomes `held`: `/readyz` answers `503` with `detail` `GOAWAY pending`, then
    `GOAWAY sent` once the delay below has passed.
  - It outranks every other state. A held node that is overloaded or has lost its validators
    still reports `held`, and so does a held node stopping, so a planned stop's last answer
    before exit is `held`.
  - `[serve] hold_goaway_delay_secs` (default 0) after the hold was seen, every gRPC connection
    gets a GOAWAY, and its in-flight streams get `max_connection_age_grace`. That covers
    connections open then and every connection accepted while still held (each GOAWAY'd at
    once). Set the delay to at least the time your load balancer takes to mark a server down,
    plus its DNS TTL.
  - The process keeps running and answering `/readyz`, `/status` and the admin listener.
- On resume, the state returns to normal evaluation, so a node that fell behind while held
  reports `syncing` first. A hold, resume and hold again counts the delay from the last hold.
- A stopped or killed zainod is never reported `held`, only absent. A dashboard can therefore
  tell a planned removal (last report `held`) from a crash.
- Under systemd, a node that starts held finishes its start job (`READY=1`) instead of waiting
  on a readiness the operator is withholding.
- Output:
  - stdout: `held` or `resumed`.
  - stderr: what changed (`hold marker written`, `already held`, `hold marker removed`,
    `was not held`).
  - `resume`, when `[metrics] listen_address` is set: the state the running zainod reports
    within 5 s, or that none answered (the marker applies when it next starts).
  - `hold`, when `[metrics] listen_address` is set: blocks until the running zainod's `/readyz`
    reports `GOAWAY sent` (at most 1 s poll + `hold_goaway_delay_secs` + 1 s + 5 s), then
    `safe to stop`; or names the state it saw instead, or that none answered.
  - `hold` without one: blocks for the longest a running zainod can take to send its GOAWAYs
    (1 s poll + `hold_goaway_delay_secs` + 1 s), counted from the command, so an old marker or
    a zainod restarted since it was written never cuts the wait short.
  - Exit 0 once the marker matches the command (both commands are idempotent), 1 when the
    config or state directory could not be read or written.

## Public health on the gRPC listener

The gRPC listener also answers two plain `GET`s, over HTTPS/1.1 or h2, for load-balancer
monitors and public dashboards; a `200` from them also proves TLS and the listener are up. Rate
limits and status codes: [zaino-grpc: serve stack](../zaino-grpc/usage.md#serve-stack).

| Path | Answers |
|---|---|
| `/readyz` | `{"state", "detail"}` as on the admin listener; `200` only when `serving` or `degraded`, else `503` |
| `/status` | the public projection below, always `200` |

The listener binds at process start, before an index-snapshot bootstrap or boot, so these paths
answer `provisioning` (with progress) or `down` from the first second. Until boot finishes,
every gRPC method answers `UNAVAILABLE`. A port already in use fails startup at once.

`/status` is safe for the open internet. It carries no validator address, peer, disk, mempool
or tailnet name:

```json
{
  "state": "serving", "detail": null, "progress": null, "notes": [],
  "version": "0.11.0-alpha.2", "network": "mainnet",
  "tip": { "height": 3515406, "hash": "0000…" },
  "served": 3515400,
  "validators": { "configured": 2, "connected": 1 },
  "indexes": { "enabled": ["utxo_set", "compact_block"], "total": 5 },
  "egress": { "start": 1790000400, "mbps": 12.5 },
  "info": { "version": "…", "blockHeight": 3515400, "…": "every GetLightdInfo field" }
}
```

- `state`, `detail`, `progress` and `notes` follow [the admin listener](#the-admin-listener)'s
  rules.
- `tip` is the verified best block and `served` is the served height.
- In `validators`, `connected` counts the answering ones (live or catching up).
- `egress` is gRPC bytes sent in the last *closed* UTC 10-minute window, in Mbit/s:
  - It is rounded to 3 significant figures, and under 0.1 is published as 0.
  - `start` is the window's start time (unix seconds).
  - The counter is read at each window boundary. The first window after boot is measured from
    boot.
- `info` holds the `GetLightdInfo` fields under lightwalletd's camelCase names, as numbers and
  strings, or `null` while that RPC would be `UNAVAILABLE`.
- Before boot, `tip`, `served`, `validators`, `egress` and `info` are `null` (`egress` also until
  its first window closes).

`/metrics` sets every state gauge from the `GrpcSnapshot` first (`zaino_best_tip`,
`zaino_fetch_height`, `zaino_index_synced{index}`, the `zaino_chainview_*` gauges:
[`zaino-grpc-snapshot`](../zaino-grpc-snapshot/usage.md#reporting)), then renders; events
(`zaino_reorgs_total`, `zaino_fetch_*_total`, histograms) are counted where they happen
([`zaino-nfs`](../zaino-nfs/usage.md#observability), [`zaino-sync`](../zaino-sync/usage.md)),
each index writer's `zaino_index_*` progress included
([writer loop](../zaino-sync/usage.md#writer-loop)). One progress task logs the sync summary and
each index's `Syncing` line (applied, durable, size) every 30 s.

The same task samples the disk every 30 s (a metric whose source is unreadable, e.g. no cgroup v2
PSI, is left out rather than reported as zero):

| Metric | Labels | Answers |
|---|---|---|
| `zaino_disk_committed_bytes` | `index`, `table` | durable data per table |
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
- The build gauge is `zaino_build_info`, with the version as a label.

## systemd

Run under `Type=notify` and zainod reports its own startup to systemd. Without `NOTIFY_SOCKET`
(any other service manager, a container, a shell) nothing is sent.

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
| `READY=1` | the first time `/readyz` passes (`serving` or `degraded`) or the node is held; nothing is sent after it |
| `EXTEND_TIMEOUT_USEC` (5 min) | each 10 s check before that which saw progress: index-snapshot bytes, the fetch height, the served height or an index height moved |

- `systemctl start` / `restart` returns only once zainod serves at the tip, so a deploy
  that restarts it (`nixos-rebuild switch`, deploy-rs) sees an upgrade that cannot catch up as
  a failed unit.
- A fresh sync or index-snapshot bootstrap keeps extending its start for as long as it moves, so
  `TimeoutStartSec` bounds only a stalled start (5 minutes without progress, once past it).
  Units ordered after zainod, and `multi-user.target` at boot, wait for that start; use
  `systemctl start --no-block` to return at once.
- `NOTIFY_SOCKET` must be a filesystem path (systemd's default), not an abstract `@` socket.

A `systemctl stop` or `restart` closes the gRPC listener at once. Open connections get
`[grpc] shutdown_grace_secs`, then each index commits what it holds. `TimeoutStopSec` must
cover both. To take a node out of rotation before a stop, hold it first (see
[Operator hold](#operator-hold)).

## Index snapshot

With `aria2c` on `PATH` (the Docker image and the Nix package ship it), zainod fills empty
indexes from a published index snapshot before it boots:

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
   "utxo_set": {"archive": "utxo_set.tar.zst", "sha256": "…", …},
   "block_hash": {…}, "tree_state": {…}, "transparent_address": {…}}}
```

Each archive is a zstd tar of that index directory's contents (no top-level directory). Keys
are the index names: `compact_block`, `utxo_set`, `block_hash`, `tree_state`,
`transparent_address` (`utxo_set` installs beside the compact-block index, which it runs
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

While it runs, the state is `provisioning`, and `/status` carries the bytes as `progress`:

```json
{"state": "provisioning", "detail": null, "notes": [],
 "progress": {"done": 27000000000, "total": 44500000000, "unit": "byte", "rate": 103000000.0}, …}
```

`total` = the fetched archives' cumulative bytes. `done` = bytes through the slowest archive's
phase (each moves downloading → verifying → unpacking), archives already past it counted whole.
`rate` bytes/s (downloading only). The `IndexSnapshot` component logs progress every 30 s:

```text
INFO  IndexSnapshot:  Downloading index snapshot  from=snapshots.zingolabs.dev to=3,501,802 indexes=5 done=27.0GB total=44.5GB rate=103MB/s eta=2m49s
INFO  IndexSnapshot:  Index snapshot installed    indexes=5 height=3,501,802 elapsed=7m12s
```

## `zainod verify`

```
zainod verify --config /etc/zaino/zainod.toml
```

Loads the same config the daemon runs with and, for every **enabled** index,
reads every file its `MANIFEST` seals against that file's page checksums
([zaino-persistence: Verifying offline](../zaino-persistence/usage.md#verifying-offline)). The same
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
  "block_hash": { "heights": 4, "units": [ … ] },
  "compact_block": {
    "heights": 4,
    "units": [
      { "name": "blocks.dat", "committed_bytes": 1234, "orphaned_bytes": 0, "lost": false, "bad_sums": false, "bad_pages": [] },
      { "name": "blocks.idx", … }
    ]
  },
  "transparent_address": null,
  "tree_state": { "heights": 4, "units": [ … ] },
  "utxo_set": { "heights": 4, "units": [ … ] }
}
```

`heights` = durable tip + 1; `units` = one entry per sealed file; `bad_pages` = 4 KiB page
indexes whose CRC disagrees; `bad_sums` = the `.crc` file no longer matches the digest the
manifest recorded; `null` = index disabled. A corrupt or lost file means delete that index's
directory and resync it.

## Failure policy

zainod dies rather than serve from a state it cannot vouch for
([zaino-persistence: the commit and crash contract](../zaino-persistence/usage.md#the-commit-and-crash-contract)):

- any panic aborts the process, including a page checksum mismatch on the read
  that first touches a corrupt page
- index writers are infallible and panic on any failure, naming the index
  ([messages](../zaino-sync/usage.md#failure-panic-never-err))
- any other task that ends first (the finalized-stream follower, the NFS, the `GrpcSnapshot`
  publisher, gRPC), cleanly or not, cancels the rest and `zainod start` exits 1
  (`IndexerError::TaskEnded` or the task's own error, e.g. `FollowError::Diverged`
  = the next finalized block does not extend an index's stored tip, as for a directory
  from another chain: delete that index's directory to resync it)
- a trusted validator going away never stops zainod: it is retried until it answers
- an index directory built for another network or format, shorter than its
  seals, or whose tail page or `.crc` file fails its checksum refuses to open: resync it

Shutdown and restarts: [running.md](../../docs/running.md#stopping-and-restarts).

## Logging

`zainod start` and `generate-config` install the global `tracing` subscriber and a panic hook
that logs the panic before aborting. `zainod verify`, `hold` and `resume` do not (stdout carries
only their report or state). Everything is read from the environment:

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
WARN  [09-28|17:29:15.175] ChainView:           Validator catching up          validator=zebrad:18232 height=3,434,171 behind=65,512 hash=00000000…1a76bf89
INFO  [09-28|17:29:42.659] ZainoNFS:            Syncing blocks                 height=3,503,023 target=3,506,659 bps=10 eta=5m49s
INFO  [09-28|17:29:42.659] CompactBlockIdx:     Syncing                        applied=3,501,900 durable=3,501,802 size=31.0GB
INFO  [09-28|17:29:42.659] BlockHashIdx:        Syncing                        applied=3,501,900 durable=3,501,802  size=134MB
```

- Each line has a 5-character level, a UTC `MM-DD|HH:MM:SS.mmm` timestamp, the
  component that logged it, and the message, padded to 30 columns when fields
  follow, so repeated lines align. Index `durable` heights and `size` are padded
  into columns (between fields, never inside a value).
- Components: `Zainod` (lifecycle), `Metrics`, `IndexSnapshot` (the `[snapshot]` bootstrap),
  `Traffic` (the balancer's driver), `ChainView` (polls folded, headers, peers, mempool,
  submission), `ZainoSync` (the finalized-stream follower), `ZainoNFS` (tip-path fetch and folds,
  the served tip, reorgs, sync progress), `GrpcSnapshot` (the snapshot publisher), `Grpc`, and
  one per index (`UtxoSetIdx`, `CompactBlockIdx`, `BlockHashIdx`, `TreeStateIdx`,
  `TransparentAddrIdx`), which also owns that index's commits and compactions. In `json` the component is the `component` field of
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
| `ChainView` | `Chain view configured` | info | At startup (`validators`, `p2p`). |
| `ChainView` | `Polling validator` | info | A validator's first listing (`validator`, `listed` = its mempool size). |
| `ChainView` | `Header chain anchored` | info | Header sync anchors its in-memory header chain (`height`, `hash`). |
| `ChainView` | `Header fetch failed` | warn | Header sync skips that validator this round and retries in 5 s (`validator`, the failure, or no member to ask: benched, down). |
| `ChainView` / `ZainoSync` / `ZainoNFS` | `Misanswer` | warn | A validator served a header that fails a rule or does not decode (header sync), or a block its verified header does not commit to (block fetch): `ticket.member`, `class`, `why`. That member is benched ([zaino-traffic: Blame](../zaino-traffic/usage.md#blame-report)); the block is asked of another. |
| `ChainView` | `Validator poll failed` / `Validator down, holds no tip` / `Validator back` | warn / warn / info | A failed poll; the member gone `Down` (its chain and sightings retracted; [polling](../zaino-traffic/usage.md#polling-and-observations)); its first answer after. |
| `ChainView` | `Transaction not accepted` | warn | A submission ended with no acceptance (`txid`, `attempts`, the rejection or failure). |
| `ChainView` | `Validator catching up` | warn | Every 60 s while a validator's mempool is off below the network tip (`validator`, its `height`, `behind` its own network estimate, `hash`). |
| `ChainView` | `Validator caught up` | info | The mempool answers again. |
| `GrpcSnapshot` | `Validator tip stale against its own clock (stalled or eclipsed)` / `Validator tip fresh again` | warn / info | The stale alarm ([§11](../../docs/architecture.md#11-telemetry)) rises, then clears (`validator`, `tip`, `behind`). |
| `GrpcSnapshot` | `Two live validators share no outbound peer (possible partition)` / `…share outbound peers again` | warn / info | The partitioned alarm rises, then clears (over `getpeerinfo`). |
| `GrpcSnapshot` | `Live validators reach few distinct outbound peers (possible eclipse)` / `…enough distinct outbound peers again` | warn / info | The eclipsed alarm rises, then clears (no outbound peer at all raises nothing). |
| `GrpcSnapshot` | `Finality paused: no trusted validator holds the boundary` / `Finality resumed` | warn / info | The finality paused alarm rises, then clears (`best`). |
| `ChainView` | `Validator peer list read failed, last kept` / `Validator release read failed, last kept` | warn | A `getpeerinfo` or `getinfo` / `getdeprecationinfo` failure (telemetry only: the poll carries on). |
| `GrpcSnapshot` | `Validator release reaches end of service soon (it halts there): upgrade it` / `Validator release upgraded` | warn / info | The end of service alarm rises (`validator`, `build`, `left` blocks), then a newer release clears it. |
| `ChainView` | `Push stream up` / `Push stream ended, polling meanwhile` | info / warn | A validator's zebra indexer push streams (`zebra_indexer`) open, or end (lag, restart, network); it reconnects on a 500 ms → 30 s ladder and polls every second meanwhile. |
| `ChainView` | `Push stream unavailable, polling` | info | Once, when the configured `indexer_address` refuses (retries then stay at debug). |
| `Grpc` / `Metrics` | `Listening` | info | At startup (`endpoint`; gRPC adds `network`). |
| `Grpc` | `Serving` | info / warn | Every 60 s while anything is served or held: `rps`, `p99` time to first message, `out` bytes/s (3 significant figures), `conns` held; warn with `failed`, `refused`, `at_capacity`, `slow`, `slowest`, `stalled`, `conns_refused` when any is non-zero ([zaino-grpc: Serving log](../zaino-grpc/usage.md#serving-log)). |
| `Grpc` | `High load` | warn | In place of `Serving` while any cap (`work`, `subs`, `conns`) is past 25% held: those caps as `used/max`. |
| `Grpc` | `Request failed` | error | A request's first server fault in a minute (`method`, `code`, `error`); later ones only counted. |
| `Grpc` | `Request unavailable` | warn | The first refusal in a minute other than a full admission pool (nothing served yet, validator unreachable: `method`, `error`). |
| `ZainoNFS` | `Syncing blocks` | info | Every 30 s (the progress task) while the indexes trail the header chain's finalized tip (`height` = the higher of the last block handed and the lowest durable index tip, `—` when neither exists; `target` = the finalized tip, `bps`, `eta`). Blocks above the finalized tip are folded by the NFS, not handed. |
| `ZainoNFS` | `Fetching non-finalized blocks` | info | Every 30 s once the indexes reach the finalized tip, while the served tip trails best and is not synced (after a restart: the window up to `depth` blocks the NFS fetches again). `fetched` / `total` = blocks above the finalized tip served / to best, `bps`, `eta`. `Serving the verified tip` marks the end. |
| `ZainoNFS` | `Block fetch stalled` / `Non-finalized block fetch stalled` | warn | The phase above has not moved for 30 s or more (`stalled` = for how long). |
| `ZainoNFS` | `Chain tip advanced` | info | Each published served tip at the verified best (`height`, `hash`, block `age`, `finalized_tip` = the header chain's finalized tip). |
| `ZainoNFS` | `Chain reorg detected` | warn | The served tip left the best chain (`from` = its height, `to` = the new served tip). |
| `GrpcSnapshot` | `Serving the verified tip` / `Behind the verified tip, syncing` | info | The `synced` judgement flips between two publishes (`height` = the served tip). |
| index | `Syncing` | info | Every 30 s while the indexes trail the finalized tip (the progress task): `applied` and `durable` (off the index writer's handle), `size` (bytes on disk, 3 significant figures, decimal units). |
| index | `Index size unreadable` | warn | A directory walk failed (`error`); `size` is left out until the next one succeeds. |
| index | `Commit waited on compaction` | warn | A commit blocked on a merge that fell two windows behind. |

Every LSM merge (`Compacting segments`, `Compacted segments`) is at `debug`.

The panic hook logs each panic as a structured `error` event at its origin —
thread, location, and message — so a panic on a worker thread that would
otherwise surface only as a distant `JoinError` is logged through the same sink
as every other error. The `panic` target is forced on so no filter drops it.
The hook then chains to `main`'s, which prints the panic and aborts the process
(see [Failure policy](#failure-policy)).
