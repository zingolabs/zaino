# zaino-persistence-lsm-tree

The on-disk backend for the `zaino-persistence` port: positional files for
sequence namespaces, an LSM tree for map namespaces, and one checksummed
manifest as the atomic commit point. Under construction: the file layer
(`fs`), page checksums, the manifest, the index directory and the LSM tree are
in place; sequences and the `Backend` adapter follow.

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
