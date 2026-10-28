# zaino-persistence-lsm-tree

The on-disk backend for the `zaino-persistence` port: positional files for
sequence namespaces, an LSM tree for map namespaces, and one checksummed
manifest per index directory as the atomic commit point. Everything goes
through the port; this guide covers what is specific to this backend. The
design is in `docs/`:
[persistence-engine.md](./docs/persistence-engine.md) (the port over the
engine), [durability.md](./docs/durability.md) (commit point, checksums,
recovery, failure policy), [lsm-deletes.md](./docs/lsm-deletes.md) and
[index-data-structures.md](./docs/index-data-structures.md) (filters).

## Opening a backend

[`LsmTreeBackend::open`] takes an [`fs::Fs`] (use
[`LsmTreeBackend::open_real`] for the real filesystem), the index directory
(created if absent) and an [`LsmTreeConfig`]:

- `name` and `format`: the index's name (at most 255 bytes) and layout
  version. Both are written into the manifest and checked at every open.
- `namespaces`: the port's `NamespaceSpec`s. The directory remembers the ones
  it was committed with, and an open declaring others is refused.
- `tuning`: per-namespace [`Tuning`], how a namespace is stored, never what it
  answers. `scope` (maps only) is the number of leading key bytes every range
  read shares, so the map's filter skips segments without that prefix (0 =
  point lookups over whole keys). `cache_writes` keeps written pages in the
  page cache (by default they are dropped once synced, so bulk writes never
  evict what readers use).
- `lsm`: the maps' [`LsmConfig`]:
  - `fanout`: same-tier segments merged into one (default 16, at least 2);
  - `merge_slots`: merges doing I/O at once (default 4);
  - `merge_mib_per_sec`: their shared bandwidth, every byte a merge reads or
    writes included (default 200);
  - `max_retries` (default 3, at most 6) and `cleanup_alert_bytes` (default
    1 GiB): removing merged-away files (below);
  - `max_merge_failures`: merges of one map that may fail in a row and be
    retried before writes stop (default 3, at least 1; below);
  - `spill_bytes`: segment index bytes (fences, filter fingerprints) a write
    or merge holds in memory before spilling them to a scratch file: memory
    against scratch I/O (default 1 MiB, at most 1 GiB).

  A setting out of range refuses the open.
- `write_buffer`: the buffered heap at which a commit writes the buffer to
  disk. Commits below it are visible to readers at once and durable after the
  next `flush` or the write that crosses it.

- `threads`: the size of the thread pool each backend builds at open for its
  writes and commit checks (`None` = the machine's available parallelism).
  Merges run on their own threads. A process opening several indexes can cap
  each one so their pools don't oversubscribe the cores. An open whose pool
  cannot start is refused.

Namespace names are `/`-separated parts of `[A-Za-z0-9_-]`, unique ignoring
case. A map owns its directory, so no namespace may sit inside a map's path.
`MANIFEST`, `LOCK`, `MANIFEST.creating` and `TAILS` are reserved, as are the Windows device
names (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`), all ignoring case.

## On disk

```text
<dir>/
  LOCK                exclusive lock for the backend's life
  MANIFEST            two 1 MiB slots: the commit point
  TAILS               two slots: each sequence file's committed tail page, restored if torn
  <sequence>.dat      records back to back (+ <sequence>.idx end offsets if Variable)
  <map>/<id>.seg      immutable sorted segments, one per write or merge
  *.crc               page checksums beside every data file
```

## Limits

- Final data only: nothing is ever updated or rewound. A map key is inserted
  once and, with `deletes`, removed at most once.
- Map keys should lead with 8 uniform bytes (a hash, a txid): the filter
  shards on them, which keeps filter-build memory bounded.
- The manifest slot holds about 32,000 map segments across all maps (32 bytes
  each). Merges keep the count logarithmic; a write that would outgrow the slot
  fails with `CommitError::ManifestFull`.
- One backend per directory at a time (the `LOCK`).

## Verifying

`Backend::verify` scrubs every committed byte against its page checksums.
[`verify_dir`] does the same offline, with no lock, so it is safe beside a live
backend; a path holding no index (no `MANIFEST`) is an error. Either covers the
files committed when it started: a file a merge retires meanwhile is skipped,
not reported lost. Either returns the port's `Verification`; anything but a
clean report means stop, verify the disk, and resync.

## Metrics

Call [`describe_metrics`] once after installing a recorder, and use
[`METRIC_BUCKETS`] and [`LSM_METRIC_BUCKETS`] for the histograms' buckets.
Every metric is labelled `index` (the index name); `zaino.lsm.*` metrics also
carry `set` (the map namespace), and the per-tier ones (`segments`, `merging`,
`merge_duration_seconds`) `tier`:

- `zaino.store.commit_seconds`, `commit_bytes_total`: durable writes.
- `zaino.store.snapshots_live`, `snapshot_heap_bytes`: memory held by
  snapshots (below).
- `zaino.lsm.segments`, `merging`, `stall_at`: each map's size tiers, and
  the segment count at which a write waits on a merge.
- `zaino.lsm.batch_rows_total`, `merge_rows_total`, `merge_bytes_total`,
  `merge_cancelled_total`, `merge_duration_seconds`,
  `stall_duration_seconds`: write amplification and merge cost.
- `zaino.lsm.leftover_bytes`, `merge_launch_failed_total`,
  `unlinked_mapped_bytes`: cleanup and disk space (below).

[`disk_bytes`] sums every file under a directory, for comparison with the
footprint the port reports.

## Memory held by snapshots

Every reader and open `scan_range` holds the state published when it was taken.
Commits since then copy the buffer nodes they change, and the old copies stay
allocated until the last reader of that state is gone. This memory is outside
`write_buffer`, which bounds only the current buffer. It grows with the number
of distinct states held at once and the commits between them, and is freed as
long-lived readers and scans finish.

Two gauges, labelled by index, show it:

- `zaino.store.snapshots_live`: published states alive (the current one plus
  those still held)
- `zaino.store.snapshot_heap_bytes`: their buffers' estimated heap, an upper
  bound (states share most nodes)

## Disk space held by readers

A merge's input segments are deleted once the merged segment is committed, but
a reader still holding an older state keeps them mapped, so their disk space is
freed only when the last such reader is gone.
`zaino.lsm.unlinked_mapped_bytes`, labelled by namespace, shows those bytes.

## Failed writes and merges

A write that fails refuses every later write (`CommitError::Poisoned`) until
the backend is reopened; readers keep reading the last published state. A
failed write may still have reached the disk: after reopening, the tip is the
last acknowledged commit or the one that failed.

A background merge that fails, or fails to start, fails the next write before
that write touches anything: nothing of it is applied, and the merge's partial
files are removed. What follows depends on why it failed:

- an I/O error a retry can clear (a create, write or read failing, a full disk,
  a thread that could not start): `CommitError::MergeFailed`, writes stay open
  and the merge runs again later;
- a corrupt or contract-breaking input, which no retry can merge:
  `CommitError::Corrupted`, writes refused until the directory is verified and
  resynced;
- a failed `fsync` of its output, a failed read-back of what it wrote, or a
  bug: `CommitError::WriteFailed`, writes refused.

After `max_merge_failures` retryable failures in a row on one tier of a map
(a merge that lands on that tier resets its count; other tiers' merges never
do), the next one refuses writes as
`CommitError::OutOfSpace` if the disk or quota is full, else as
`CommitError::WriteFailed`.

Corruption found while checking a commit's keys fails it with
`CommitError::Corrupted` and refuses later writes.

Files a merge replaced are removed after the commit that replaced them, as are
scratch files a write or merge spilled to and could not remove. A removal that
fails never fails the commit: every failing file is retried
together, up to `max_retries` rounds (10, 20, 40 ms … apart, so a write pauses
at most their sum), then once at every later write, with
`zaino.lsm.leftover_bytes` showing what is left. Each such write logs a
warning, or an error once the leftovers across all maps reach
`cleanup_alert_bytes` (default 1 GiB), since the disk may be failing. An open
that cannot remove them is refused.

## Memory-mapped reads and SIGBUS

Committed files are read through read-only memory maps: zero-copy, no system
call per read, and no locks, which is what lets many concurrent readers scale.
The cost is one failure mode that cannot be turned into an error:

- **SIGBUS kills the process** if a mapped page cannot be read. That happens
  when a committed file is truncated by something outside this crate, or when
  the disk returns a read error while the kernel faults a page in.
- This crate never truncates a committed file, sealed segments are made
  read-only (`0o444`), and the directory's `LOCK` keeps other Zaino processes
  out. A process running as root, or one that changes the permissions back, can
  still truncate them.

Operate accordingly: keep the data directory on a dedicated, healthy disk, and
never modify, copy over or truncate its files while a backend has it open.
After a SIGBUS, check the disk, verify the directory, and resync if it reports
damage.

## Reopening after a failed write

When the kernel fails to flush a write, it may keep the unflushed bytes in the
page cache as if they were written. Open therefore reads the `MANIFEST` (the
commit point) from the device itself, bypassing the cache: every file the
on-disk manifest lists was flushed successfully before it was written. This is
guaranteed on Linux (`O_DIRECT`). On tmpfs, which has no device behind its
cache, the manifest is read plainly; on any other Linux filesystem that refuses
`O_DIRECT` the open is refused. On macOS it is best effort (`F_NOCACHE` leaves
pages already cached in place), so restart the process after a failed write
there.

An open refused because the directory belongs to another name, format or
namespaces is `OpenError::Mismatch`; one that finds committed data lost or
corrupt is `OpenError::Corrupted`.
