# zaino-persistence-lsm-tree

The on-disk backend for the `zaino-persistence` port: positional files for
sequence namespaces, an LSM tree for map namespaces, and one checksummed
manifest as the atomic commit point. Under construction: the file layer
(`fs`), page checksums, the manifest, the index directory and the LSM tree are
in place; sequences and the `Backend` adapter follow.

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

## Failed merges

A background merge that fails, or fails to start, fails the next write with
`CommitError::MergeFailed`: nothing of that write is applied, the merge's
partial files are removed, and it runs again later. A merge whose input is
corrupt can never succeed: that write fails with `CommitError::Corrupted` and
writes are refused until the directory is verified and resynced.

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
guaranteed on Linux (`O_DIRECT`); on macOS it is best effort (`F_NOCACHE`
leaves pages already cached in place), so restart the process after a failed
write there.
