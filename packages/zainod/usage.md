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

`/readyz` reasons: `starting` (indexer not booted), `heartbeat_stale`, `no_quorum_tip`,
`<index>_syncing` (an enabled index's serving gate is closed).

`/statusz` = version, network, uptime, readiness, the quorum tip (`agreed` of `configured`,
`threshold`), `fetch_height` (the last block fetched: sync progress between index batch
commits, where `applied` moves in jumps), each configured validator (state, agreement, tip height, stale blocks, latency,
failures, the p2p peers its `getpeerinfo` reports), the chainview alarms, and every index (enabled,
synced, three heights, bytes on disk and per subdirectory, requests answered). Heights:
`durable` = committed to disk; `merged` = the last final block held in memory for the next bulk
commit (`batch_mib`), `null` once committed; `applied` = the highest block served (the in-memory
view tip, ≥ `durable`). Bulk sync moves `merged` per block and `durable` per batch; at the tip,
each finalized block moves `durable` by one.
Index sizes come from the last status-line walk, so they are absent until the first one
(2 minutes while syncing). `grpc.sent_bytes` = response body bytes served, all methods.
Request counts make it traffic data: keep the listener private.

- A supervised task on the serving runtime republishes the heartbeat every 100ms.
- The listener binds before the recorder installs, so a bind failure fails startup.
- At most 32 admin connections are served at once; the rest wait in the accept backlog.
- A non-private `listen_address` logs a warning at startup, because `/metrics` is
  unauthenticated.
- The build gauge keeps its released name, `zainod_build_info`, with the version as a label.

## Index snapshot

Built with the `snapshot` feature (`cargo build --features snapshot`, Nix
`zainod.override { features = [ "snapshot" ]; }`, Docker `--build-arg CARGO_FEATURES=snapshot`)
and `aria2c` on `PATH`, zainod fills empty indexes from a published snapshot before it boots:

```toml
[snapshot]
manifest = "https://snapshots.zingolabs.dev/zaino-snapshot-1.0.0.json"
connections = 8   # parallel connections (aria2c --split), at most 16
```

The manifest describes one archive (`archive` resolves against the manifest URL):

```json
{"archive": "zaino-snapshot-1.0.0.tar.zst", "bytes": 44500000000,
 "sha256": "…", "height": 3501802, "network": "mainnet"}
```

The archive is a zstd tar holding one top-level directory per index, named like its default
path: `compact-block`, `value-balance`, `block-hash`, `tree-state`, `transparent-address`.

- Only an enabled index whose directory is missing or empty is filled; one holding data is
  never touched, and with none empty the snapshot is skipped (no download).
- aria2c downloads over its JSON-RPC (loopback, random secret): segmented, retried without
  limit on transient errors, resumed after a restart. It stops itself if zainod dies.
- The archive is checked against `sha256`, unpacked into `.zaino-snapshot` beside the
  compact-block index, then each index directory is renamed into place. Every index path must
  share that filesystem.
- A restart resumes at the step it stopped in (download, verify, unpack, install).
- A manifest for another network, a hash mismatch or a permanent download error stops startup.

While it runs, `/readyz` answers `snapshot_downloading` / `snapshot_verifying` /
`snapshot_unpacking`, and `/statusz` carries the progress:

```json
{"ready": false, "reasons": ["snapshot_downloading"],
 "snapshot": {"phase": "downloading", "source": "snapshots.zingolabs.dev", "height": 3501802,
              "done": 27000000000, "total": 44500000000, "rate": 103000000}}
```

`done` / `total` are bytes, `rate` bytes/s (downloading only). The `Snapshot` component logs each
phase, and progress every 30 s:

```text
INFO  Snapshot:  Downloading index snapshot  from=snapshots.zingolabs.dev to=3,501,802 done=27.0GB total=44.5GB rate=103MB/s eta=2m49s
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
- index loops are infallible and panic on any failure. A failed commit names
  the index and its directory: `<index> index commit failed: disk <dir> full`,
  or `… failed at <dir>: <error>`
  ([zaino-sync: Failure](../zaino-sync/usage.md#failure-panic-never-err))
- any other task that ends first (producer, chainview poller, gRPC), cleanly or
  not, cancels the rest and `zainod start` exits 1 (`IndexerError::TaskEnded` or
  the task's own error)
- there is no in-process restart, so run it under a service manager that
  restarts it
- an index directory built for another network or format, shorter than its
  seals, or whose tail page fails its checksum refuses to open: resync it
- `fetch.finalised_depth` defaults to Zebra's reorg bound (1000,
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
INFO  [09-28|17:29:42.659] ZainoSource:         Syncing blocks                 height=3,503,023 target=3,506,659 bps=10 eta=5m49s
INFO  [09-28|17:29:42.659] CompactBlockIdx:     Syncing                        durable=3,501,802 merged=3,503,019 applied=3,501,802 size=31.0GB
INFO  [09-28|17:29:42.659] BlockHashIdx:        Syncing                        durable=3,501,802 merged=3,503,019 applied=3,501,802  size=134MB
```

- Each line has a 5-character level, a UTC `MM-DD|HH:MM:SS.mmm` timestamp, the
  component that logged it, and the message, padded to 30 columns when fields
  follow, so repeated lines align. Index `durable` / `merged` / `applied` heights and `size`
  are padded into columns (between fields, never inside a value).
- Components: `Zainod` (lifecycle), `Metrics`, `ChainView` (validator polling,
  mempool), `ZainoSource` (bulk block fetch), `ZainoNFS` (following the chain tip, reorgs), `Grpc`, and one per index
  (`CompactBlockIdx`, `ValueBalanceIdx`, `BlockHashIdx`,
  `TreeStateIdx`, `TransparentAddrIdx`), which also owns that index's
  commits and compactions. In `json` the component is the `component` field of
  the event's enclosing span.
- Fields are `key=value` logfmt, parseable by Loki and `hl`. Integers from
  1,000 up, heights included, are grouped with commas (`json` keeps them plain
  numbers). A 64-hex hash shows its first and last 8 digits (`json` keeps it
  whole). A value containing a space or `=` is quoted, and an error field
  carries its whole source chain. A path in a message longer than 48 columns
  keeps its tail (`…/zaino/compact-block`).
- Fields from enclosing spans follow the event's own fields.

What an operator sees at `info`:

| Component | Event | Level | When |
|---|---|---|---|
| `ChainView` | `Validator reachable` / `Quorum configured` | info | At startup. |
| `ChainView` | `Validator catching up` | warn | Every 60 s while a validator's mempool is off below the network tip (`endpoint`, its `height`, `behind` its own network estimate, `hash`). |
| `ChainView` | `Validator caught up` | info | The mempool answers again. |
| `ChainView` | `Validator tip stale against its own clock (stalled or eclipsed)` / `Validator tip fresh again` | warn / info | A live validator's tip falls ≥ 24 blocks behind its own `estimatedheight`, then recovers (`endpoint`, `tip`, `estimated`). |
| `ChainView` | `Two live validators share no outbound peer (possible partition)` / `…share outbound peers again` | warn / info | Edge of the partition check over `getpeerinfo`. |
| `ChainView` | `Live validators reach few distinct outbound peers (possible eclipse)` / `…enough distinct outbound peers again` | warn / info | Edge of the eclipse check (1 to 2 distinct outbound peers across live validators; none at all raises nothing). |
| `ChainView` | `Peer list read failed, last one kept` | warn | A `getpeerinfo` transport failure (telemetry only: the poll carries on). |
| `ZainoNFS` | `Quorum tip below the non-final window (agreeing validators lag), waiting` | warn | Chainview's tip retreated under the chain head's window (`tip`, `floor`); production resumes on the next tip. |
| `Grpc` / `Metrics` | `Listening` | info | At startup (`endpoint`; gRPC adds `network`). |
| `Grpc` | `Serving` | info / warn | Every 60 s while anything is served or held: `rps`, `p99` time to first message, `out` bytes/s (3 significant figures), `conns` held; warn with `failed`, `refused`, `at_capacity`, `slow`, `slowest`, `stalled`, `conns_refused` when any is non-zero ([zaino-grpc: Serving log](../zaino-grpc/usage.md#serving-log)). |
| `Grpc` | `High load` | warn | In place of `Serving` while any cap (`streams`, `subs`, `conns`) is past 25% held: those caps as `used/max`. |
| `Grpc` | `Request failed` | error | A request's first server fault in a minute (`method`, `code`, `error`); later ones only counted. |
| `Grpc` | `Request unavailable` | warn | The first refusal in a minute other than a full admission pool (index syncing, validator unreachable: `method`, `error`). |
| `ZainoSource` | `Syncing to finalized target` | info | A bulk pass starts (`from`, `target` = tip − `finalised_depth`, `tip`). |
| `ZainoSource` | `Syncing blocks` | info | Every 30 s during a bulk pass (`height` fetched, `target`, `bps`, `eta`). |
| `ZainoSource` | `Block fetch stalled` | warn | A whole 30 s interval of a bulk pass added no block. |
| `ZainoSource` | `Reached finalized target` | info | A bulk pass finished (`blocks`, `elapsed`, average `bps`). |
| `ZainoSource` | `Applying to tip` | info | Every 30 s from there until the non-final window reaches the tip it saw (`applied`, `tip`, `bps`, `eta`). |
| `ZainoNFS` | `Chain tip advanced` | info | Each chain-head step past bulk (`height`, `hash`, `blocks`, `txs`, block `age`, `finalized`). |
| `ZainoNFS` | `Chain reorg detected` | warn | A branch won (`fork`, `dropped`, `added`, new tip). |
| index | `Syncing` | info | Every 30 s in bulk sync: `durable` (on disk), `merged` (last final block held for the next bulk commit, `—` when none), `applied` (highest block served), `size` (bytes on disk, 3 significant figures, decimal units). |
| index | `Committed bulk` | info | Once, as the index hands bulk over to its non-final window (`durable`, `size`). |
| index | `Serving` | info | The serving gate opens (`durable`, `applied`, `size`). |
| index | `Syncing, requests refused` | info | The serving gate closes (`height` = applied tip). |
| index | `Index size unreadable` | warn | A directory walk failed (`error`); `size` is left out until the next one succeeds. |
| index | `Reorg received, replaying` | warn | A reset dropped the non-finalized state (`durable` = the tip it replays from, `dropped` = blocks discarded); requests are refused until it replays. |
| index | `Reorg replayed, serving` | info | The replay reached the tip and the gate reopened (`height`, `took` = from the reset). |
| index | `Commit waited on compaction` | warn | A commit blocked on a merge that fell two windows behind. |

Per-commit lines (`Committed batch`) and every LSM merge (`Compacting segments`,
`Compacted segments`) are at `debug`.

The panic hook logs each panic as a structured `error` event at its origin —
thread, location, and message — so a panic on a worker thread that would
otherwise surface only as a distant `JoinError` is logged through the same sink
as every other error. The `panic` target is forced on so no filter drops it.
The hook then chains to `main`'s, which prints the panic and aborts the process
(see [Failure policy](#failure-policy)).
