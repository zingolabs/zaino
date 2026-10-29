# `zainod` — usage

The daemon binary: config → boot → one task per stage. Operating it (configuration, states,
hold, systemd, index snapshots, `verify`, network exposure) is [`docs/running.md`](../../docs/running.md).

## Layout

- `config/`: the file's `[…]` tables (`DaemonConfigFile`) resolved once, at load, into
  `DaemonConfig` (paths set, enabled indexes, validator endpoints, every check passed): the one
  shape `start` and every subcommand run on. Tables land in each crate's own config types; the
  only place daemon config crosses into the stack crates.
- `daemon/`: boot and pipeline composition. The pipeline's shape is drawn once, in
  `daemon/mod.rs`. Every stage is one task in a `JoinSet`; the first to end ends the daemon.
- `health/`: the state rules (`readiness`, pure), `/readyz`, `/status`, the admin listener,
  systemd notify and the state log.
- `telemetry/`: logging, the metrics recorder, disk sampling, sync progress, gRPC egress.
- `command/`: `verify`, `hold` / `resume`, `overload`.

## The admin listener

Runs on its own thread and current-thread runtime: a probe answered from a saturated serving
runtime would measure that runtime's queue, and a timed-out liveness probe gets the pod killed.

- A supervised task on the serving runtime beats the heartbeat every 100 ms; `/livez` fails after
  30 s without one.
- It binds before the metrics recorder installs, so a bind failure fails startup.
- At most 32 admin connections are served at once; the rest wait in the accept backlog.
- Each scrape sets every state gauge into a recorder of its own, from one `GrpcSnapshot` load
  (G9), so a label set the state dropped leaves the scrape.

## Failure policy

zainod dies rather than serve from a state it cannot vouch for
([zaino-persistence: the commit and crash contract](../zaino-persistence/usage.md#the-commit-and-crash-contract)):

- Any panic aborts the process (`main`'s hook prints it to stderr first), including a page
  checksum mismatch on the read that first touches a corrupt page.
- Index writers are infallible and panic on any failure, naming the index
  ([messages](../zaino-sync/usage.md#failure-panic-never-err)).
- Any other task that ends first, cleanly or not, cancels the rest and `zainod start` exits 1
  (`IndexerError::TaskEnded` or the task's own error). `FollowError::Diverged` = the next
  finalized block does not extend an index's stored tip, as for a directory from another chain:
  delete that index's directory to resync it.
- An index directory built for another network or format, shorter than its seals, or whose tail
  page or `.crc` file fails its checksum refuses to open: resync it.
- A trusted validator going away never stops zainod.

## Logging

`zainod start` and `generate-config` log [logfmt](https://brandur.org/logfmt) to stdout, coloured only on a terminal;
`verify`, `hold`, `resume` and `overload` print only their report.
`RUST_LOG` is the standard tracing filter: unset, zaino crates log at `info`; a malformed one
stops startup.

```text
ts=… level=info target=zainod::telemetry::progress message="Syncing blocks" height=3503023 target=3506659 bps=10 eta=5m49s component=ZainoNFS
ts=… level=info target=zainod::telemetry::progress message=Syncing applied=3501900 durable=3501802 size=31.0GB component=compact_block
```

- `component` names the stage: `Zainod`, `Admin`, `IndexSnapshot`, `Traffic`, `ChainView`,
  `ZainoSync`, `ZainoNFS`, `GrpcSnapshot`, `Grpc`, and one per index named as its config table.
- WARN and ERROR are worth an alert; `info` is progress and lifecycle.
- `State changed` logs each change of the [state](../../docs/running.md#states) within a second,
  at the new state's level (`down` error, `held` / `degraded` warn, the rest info). `Not ready`
  repeats it every 60 s while `down`, `degraded` or `held`.
- Alarms ([architecture §11](../../docs/architecture.md#11-telemetry)) log WARN once when raised
  and INFO once when cleared.
- Every 30 s while syncing: `Syncing blocks` (the finalized path), `Fetching non-finalized blocks`
  (the NFS window after a restart), and one `Syncing` line per index; `Block fetch stalled` /
  `Non-finalized block fetch stalled` warn when a phase stops moving. `Serving the verified tip`
  marks the end.
- gRPC logs one `Serving` summary a minute, WARN when anything failed or was refused, plus the
  minute's first `Request failed` and `Request unavailable`.
- LSM merges log at `debug`.

## Tests

`daemon/mod.rs` tests the task policy (a task ending first cancels the rest and fails the daemon,
a signal stops it clean, during boot too) and the whole pipeline over a `MockValidator` and gRPC
on localhost (`a_reorg_serves_the_heavier_chain_and_every_rpc_agrees_on_the_served_tip`). The state
rules, `/status`, systemd notify, hold, overload markers and `verify` test beside their code.
