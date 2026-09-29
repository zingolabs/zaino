# `zainod` — usage

The daemon binary: its offline disk check, failure policy, logging and admin listener.
Configuration and running the daemon are in [`docs/running.md`](../../docs/running.md).

## The admin listener

With `metrics_endpoint` set, zainod serves an admin listener on
its own thread and current-thread runtime. A probe answered from a saturated serving runtime
would measure that runtime's queue, and a timed-out liveness probe gets the pod killed.

| Path       | Answers                                | Fails when                     |
| ---------- | -------------------------------------- | ------------------------------ |
| `/metrics` | Prometheus exposition, process metrics | render panic (`500`)           |
| `/livez`   | the serving runtime still schedules    | no heartbeat for 30s (`503`)   |

- A supervised task on the serving runtime republishes the heartbeat every 100ms.
- The listener binds before the recorder installs, so a bind failure fails startup.
- At most 32 admin connections are served at once; the rest wait in the accept backlog.
- A non-private `metrics_endpoint` logs a warning at startup, because `/metrics` is
  unauthenticated.
- The build gauge keeps its released name, `zainod_build_info`, with the version as a label.

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
crash's leftovers) are counted as `orphaned_bytes`, never as corruption.

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
      { "path": "blocks.dat", "committed_bytes": 1234, "orphaned_bytes": 0, "lost": false, "bad_pages": [] },
      { "path": "offsets.idx", … }
    ]
  },
  "block_hash": { "heights": 4, "files": [ … by_hash/*.seg ] },
  "tree_state": { "heights": 4, "files": [ … heights.idx, then per pool l00.dat … l31.dat, subtrees.dat ] },
  "transparent_address": { "heights": 4, "files": [ … receives/*.seg, spent/*.seg ] }
}
```

`bad_pages` = 4 KiB page indexes whose CRC disagrees. A corrupt or lost file
means delete that index's directory and resync it.

## Failure policy

zainod dies rather than serve from a state it cannot vouch for
([`docs/design/durability.md`](../../docs/design/durability.md) §6):

- any panic aborts the process, including a page checksum mismatch on the read
  that first touches a corrupt page
- the first task to end, cleanly or not, cancels the rest and `zainod start`
  exits 1 (`IndexerError::TaskEnded` or the task's own error); there is no
  in-process restart, so run it under a service manager that restarts it
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
INFO  [09-28|17:29:12.660] CompactBlockIdx:     Opening from /var/lib/zaino/compact-block
WARN  [09-28|17:29:15.175] ChainView:           Validator catching up         endpoint=zebrad:18232 height=3,434,171 behind=65,512 hash=00000000…1a76bf89
INFO  [09-28|17:29:42.659] ZainoSource:         Syncing blocks                height=31,399 target=3,433,171 synced=0.91% bps=1,047 tps=6,077 eta=54m10s
```

- Each line has a 5-character level, a UTC `MM-DD|HH:MM:SS.mmm` timestamp, the
  component that logged it, and the message, padded to 30 columns when fields
  follow, so repeated lines align.
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
| index | `Opening from <path>` | info | At startup, per enabled index. |
| `ChainView` | `Validator reachable` / `Quorum configured` | info | At startup. |
| `ChainView` | `Validator catching up` | warn | Every 60 s while a validator's mempool is off below the network tip (`endpoint`, its `height`, `behind` its own network estimate, `hash`). |
| `ChainView` | `Validator caught up` | info | The mempool answers again. |
| `Grpc` / `Metrics` | `Listening` | info | At startup (`endpoint`; gRPC adds `network`). |
| `ZainoSource` | `Syncing to finalized target` | info | A bulk pass starts (`from`, `target` = tip − `finalised_depth`, `tip`). |
| `ZainoSource` | `Syncing blocks` | info | Every 30 s during a bulk pass (`height`, `target`, `synced`, `bps`, `tps`, `eta`). |
| `ZainoSource` | `Block fetch stalled` | warn | A whole 30 s interval of a bulk pass added no block. |
| `ZainoSource` | `Reached finalized target` | info | A bulk pass finished (`blocks`, `elapsed`, average `bps`). |
| `ZainoNFS` | `Chain tip advanced` | info | Each chain-head step past bulk (`height`, `hash`, `blocks`, `txs`, block `age`, `finalized`). |
| `ZainoNFS` | `Chain reorg detected` | warn | A branch won (`fork`, `dropped`, `added`, new tip). |
| index | `Serving` / `Syncing, requests refused` | info | The index's serving gate changes. |
| index | `Index on disk` | info | Every 120 s while that index syncs, silent once it serves (`durable` tip, omitted while empty; `size` = every file in its directory). |
| index | `Index size unreadable` | warn | That interval's directory walk failed (`durable`, `error`); the next one retries. |
| index | `Commit waited on compaction` | warn | A commit blocked on a merge that fell two windows behind. |

Per-commit lines (`Committed batch`) and every LSM merge (`Compacting segments`,
`Compacted segments`) are at `debug`.

The panic hook logs each panic as a structured `error` event at its origin —
thread, location, and message — so a panic on a worker thread that would
otherwise surface only as a distant `JoinError` is logged through the same sink
as every other error. The `panic` target is forced on so no filter drops it.
The hook then chains to `main`'s, which prints the panic and aborts the process
(see [Failure policy](#failure-policy)).
