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
- [`BackendReader`] — `get` one key, `scan` a whole namespace, or `scan_range`
  a key range in lazily read chunks.
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

## Range scans

`BackendReader::scan_range` walks the keys of a namespace within a
[`ScanRange`] — lower and upper `Bound`s, a [`ScanDirection`], and a
`chunk_bytes` budget — and returns a lazy iterator of chunks. Each `next()`
reads one chunk, so memory is bounded by the budget (or one oversized entry),
not the range, and a consumer that stops pulling stops the reads.

The filter is called once per entry on the borrowed key and value bytes and
returns `Option<T>`: `None` skips the entry, `Some` keeps it. Decode in the
filter to avoid an intermediate copy; pass [`raw_entry`] to keep owned bytes.

```rust,ignore
use std::num::NonZeroUsize;
use std::ops::Bound;
use zaino_persistence::{raw_entry, BackendReader, ScanDirection, ScanRange};

let range = ScanRange {
    lower: Bound::Included(start_key),
    upper: Bound::Included(end_key),
    direction: ScanDirection::Reverse,
    chunk_bytes: NonZeroUsize::new(4 << 20).expect("non-zero"),
};
for chunk in reader.scan_range(ns, range, raw_entry) {
    serve(chunk?);
}
```

Chunks are never empty, and the iterator ends after the range is exhausted or
after an `Err`. Each chunk reads one consistent state; successive chunks may
observe later commits, but no key is returned twice. The iterator is
`Send + 'static`, so an async caller can move it into a blocking task per chunk.

## Errors

One error type per operation — [`OpenError`], [`CommitError`], [`ReadError`],
[`FlushError`] — each preserving the backend's own failure as a boxed cause while
naming no concrete backend type. [`CommitError`] additionally distinguishes
`OutOfSpace`, which no retry can clear on its own.

## Backends

Every backend rejects a namespace it was not constructed with: reads fail with
`ReadError::NamespaceNotFound`, and a commit naming one fails with
`CommitError::NamespaceNotFound` and applies nothing. A declared namespace with
no entries reads as empty.

The `in_memory` backend (behind the `testing` feature) is a correct,
IO-free implementation for tests, benchmarks, and ephemeral runs, constructed
with its namespaces (`InMemoryBackend::new(&[ns])`); it also ships a
latency-injecting wrapper for measuring commit cost. On-disk backends (LMDB) live
in their own adapter crates.
