# zaino-backend-lmdb

The LMDB adapter for the [`zaino-persistence`](../zaino-persistence/usage.md)
port: one named database per namespace, atomic batch commits in a write
transaction, memory-mapped reads. It is the daemon's on-disk store; index code
depends on the port, never on this crate.

The environment opens with `NO_SYNC`, so a commit does not fsync — durability is
forced at batch boundaries through `flush`, and the watermark a batch stamps lets
a crash between flushes resume cleanly. Every typed error carries the underlying
`lmdb::Error` as a boxed source; a full map (`MDB_MAP_FULL`) is surfaced as the
distinct `CommitError::OutOfSpace` so a caller can grow the map rather than treat
a capacity limit as corruption.

## Key order and append

Each namespace is opened with its `KeyOrder`. A `WalkOrdered` namespace — its
keys lead with the big-endian height — is written with `MDB_APPEND`: LMDB skips
the B-tree search, fills pages sequentially, and rejects a key that is not
strictly ascending with `CommitError::OutOfOrderAppend` naming the namespace, so
a mis-stated key order fails on its first out-of-order batch rather than
mis-ordering silently. A `Scattered` namespace takes plain overwriting puts.

## Deferral

Hash-keyed (`Scattered`) namespaces land each batch on scattered B-tree leaves,
so once the tree outgrows memory every insert rewrites a leaf page for a few
bytes of payload — the cost that makes a first mainnet sync of the heavy indexes
far slower than the compact set. In **bulk mode** (`begin_bulk` with an enabled
policy) this backend defers those namespaces: each commit sorts the batch,
appends one framed segment to a per-namespace run log under `<store>/deferred/`,
fsyncs it, and records the committed length in a manifest entry written *in the
same transaction as the watermark*. `finish_bulk` then k-way merges the segments
and loads them with one ordered `APPEND` pass. The mechanism — the routing rule,
the segment format, the manifest and the resumable merge — is specified in the
`deferred` module's documentation; this guide states only the operator-facing
contract.

**Crash semantics.** The manifest, committed with the watermark, is the single
source of truth. Bytes on a run log past its recorded length belong to a batch
whose watermark never committed, so they are truncated on reopen and the batch
replays. A crash mid-merge resumes from the last chunk's recorded mark, skipping
keys already loaded, so no key is dropped or duplicated. `bulk_pending` reports an
unfinished load left by such a crash, and `is_complete` reads `false` for a
namespace still being merged — which the serving layer maps to not-yet-serviceable
rather than serving partial data.

**Disk budget.** The run logs hold the raw entries of the deferred namespaces,
roughly their final on-disk size minus B-tree slack (tens of GB on mainnet).
During `finish_bulk` both a log and its growing tree exist, so peak extra disk is
about one log's size, released as each namespace finishes and its log is deleted.
The write amplification is gone: each log is written once sequentially, read once,
and the tree written once in order.

`finish_bulk` logs its per-namespace start and end at `info` — a merge can run
for minutes — with the entry count, chunk count and wall time. The `sync-profile`
feature adds the per-commit put/flush split and periodic environment statistics;
it is off by default and compiled out otherwise.
