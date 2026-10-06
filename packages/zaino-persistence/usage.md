# zaino-persistence

The driven-port seam between index logic and the concrete key-value store: one
read/write interface that the sync engine (writer) and the serving layer (reader)
both depend on, implemented by backend adapters (LMDB, in-memory). Index code
never names a concrete store — it writes [`WriteOp`]s into a [`Namespace`] and
commits a batch atomically, so a partially-applied write can never be observed.
Callers stay backend-agnostic at compile time; the atomic batch commit holds at
runtime.

## The port

- [`Backend`] — opens reader and writer handles and forces durability.
- [`BackendReader`] — `get` one key, or `scan` a whole namespace.
- [`BackendWriter`] — `commit` a batch of [`WriteOp`]s atomically.

A [`Namespace`] names an independent keyspace within the backend (a named
database in LMDB, a column family in RocksDB, a separate map in memory). Keys and
values are raw bytes ([`RawKey`], [`RawValue`]) that the backend stores and
returns without interpreting; the index schema owns their encoding and their
lexicographic ordering.

```rust,ignore
use zaino_persistence::{Backend, BackendWriter, Namespace, WriteOp};

let ns = Namespace::new("blocks");
let mut writer = backend.writer()?;
writer.commit(vec![WriteOp::Put {
    namespace: ns,
    key: encoded_key,
    value: encoded_value,
}])?;
```

## Errors

One error type per operation — [`OpenError`], [`CommitError`], [`ReadError`],
[`FlushError`] — each preserving the backend's own failure as a boxed cause while
naming no concrete backend type. [`CommitError`] additionally distinguishes
`OutOfSpace`, which no retry can clear on its own.

## Backends

The `in_memory` backend (behind the `testing` feature) is a correct,
IO-free implementation for tests, benchmarks, and ephemeral runs; it also ships a
latency-injecting wrapper for measuring commit cost. On-disk backends (LMDB) live
in their own adapter crates.
