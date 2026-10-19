# zaino-persistence

The driven-port seam between index logic and the concrete key-value store: one
read/write interface that the sync engine (writer) and the serving layer (reader)
both depend on, implemented by backend adapters (the on-disk LSM tree, in-memory).
Index code never names a concrete store — it writes [`WriteOp`]s into a
[`Namespace`] and commits a batch atomically at a block tip, so a partially-applied
write can never be observed. Callers stay backend-agnostic at compile time; the
atomic batch commit and the commit rules hold at runtime.

## The port

- [`Backend`] — opens reader and writer handles, makes commits durable
  (`flush`), checks stored integrity (`verify`) and reports size per namespace
  (`footprint`).
- [`BackendReader`] — a snapshot: its `tip`, `get` one key, or `scan_range` a
  key range in lazily read chunks. Everything a client-serving path needs.
- [`BackendWriter`] — `commit` a batch of [`WriteOp`]s atomically at a
  [`BlockRef`](zaino_primitives::types::BlockRef) tip.
- [`MaintenanceReader`] — `scan` a whole namespace into memory, for
  maintenance, tests and tooling only. Its cost grows with the namespace, not
  the request: never use it to serve clients, where many concurrent callers
  could exhaust memory.

Keys and values are raw bytes ([`RawKey`], [`RawValue`]) that the backend stores
and returns without interpreting; the index schema owns their encoding and their
lexicographic ordering.

## Namespaces

Every namespace is declared at construction by a [`NamespaceSpec`], one of two
shapes ([`NamespaceKind`]):

- **Sequence** — records by position, each of the declared [`Width`] (fixed or
  variable). A record's key is [`sequence_key`]`(position)`, big-endian, so key
  order is position order. Append-only.
- **Map** — fixed-width keys to fixed-width values. Each key is inserted at most
  once; with `deletes`, removed at most once and never re-inserted.

```rust,ignore
use std::num::NonZeroU32;
use zaino_persistence::{sequence_key, BackendWriter, Namespace, NamespaceSpec, Width, WriteOp};

const BLOCKS: Namespace = Namespace::new("blocks");
const BY_HASH: Namespace = Namespace::new("by_hash");
let specs = [
    NamespaceSpec::sequence(BLOCKS, Width::Variable),
    NamespaceSpec::map(BY_HASH, NonZeroU32::new(32)?, NonZeroU32::new(4)?, false),
];

let mut writer = backend.writer()?;
writer.commit(tip, vec![
    WriteOp::Put { namespace: BLOCKS, key: sequence_key(height), value: block_bytes },
    WriteOp::Put { namespace: BY_HASH, key: hash.to_vec(), value: height_bytes },
])?;
```

## Commit rules

Every backend checks every batch with [`check_commit`] before applying any of it;
a batch breaking a rule fails with that rule's [`CommitError`] and applies
nothing.

- The tip is strictly above the last committed tip (`TipNotAbove`): the store
  holds final data only, with no rollback.
- Every namespace is declared (`NamespaceNotFound`).
- Sequence: `Put` at the next position only (`NotNextPosition`), records of the
  declared width (`WidthMismatch`), no `Delete` (`DeleteNotAllowed`).
- Map: keys and values of the declared widths (`WidthMismatch`); each key once
  per batch (`DuplicateInBatch`); `Put` of an absent key (`KeyExists`); `Delete`
  of a present key (`KeyAbsent`) in a namespace declared with `deletes`
  (`DeleteNotAllowed`).
- Re-inserting or re-deleting a removed key is a caller error, refused with
  `KeyRemoved` while the backend still records the removal. A backend may forget
  a removal once nothing of the key is stored (the LSM tree does, after a merge).

A backend implements [`CommitState`] (its tip, each sequence's length, each map
key's [`KeyState`]) and calls [`check_commit`]; [`check_specs`] refuses a
namespace declared twice.

## Visibility and durability

Each reader handle is a snapshot: every read through it, `tip` included, sees
the state as of `Backend::reader`, so its data always belongs to its tip. A
commit is visible to every reader opened after `commit` returns; a held reader
never sees it. A commit is durable after `Backend::flush`, or earlier if the
backend writes on its own (e.g. when its buffer fills). After a crash or reopen
the store holds a prefix of the commits: everything flushed, and possibly more.

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
after an `Err`. Every chunk reads the reader's snapshot, and no key is returned
twice. The iterator is `Send + 'static`, so an async caller can move it into a
blocking task per chunk.

## Verification and footprint

`Backend::verify` checks that the stored bytes are exactly the bytes last made
durable, and returns a [`Verification`]: one [`Unit`] per stored file (or the
backend's equivalent) with its [`Problem`]s — missing, truncated, a page or the
checksum list not matching, or harmless orphaned bytes past the commit. It checks
integrity, not meaning. `Backend::footprint` returns a [`NamespaceFootprint`] per
declared namespace: bytes stored and, where knowable, bytes in the page cache.

## Errors

One error type per operation — [`OpenError`], [`CommitError`], [`ReadError`],
[`FlushError`], [`VerifyError`], [`FootprintError`] — each preserving the
backend's own failure as a boxed cause while naming no concrete backend type.
[`CommitError`] distinguishes each broken rule, `OutOfSpace` and `ManifestFull`
(no retry clears them on their own) and `Poisoned` (a write failed earlier:
reopen). [`ReadError::Corrupted`] means stored bytes failed their integrity
check: stop, verify, resync. A `get` whose key is not of the namespace's key
width (8 bytes for a sequence) fails with `ReadError::WidthMismatch` rather than
reading as absent; `scan_range` bounds may be any length.

## Backends

Every backend rejects a namespace it was not constructed with: reads fail with
`ReadError::NamespaceNotFound`, and a commit naming one fails with
`CommitError::NamespaceNotFound` and applies nothing. A declared namespace with
no entries reads as empty.

Behind the `testing` feature:

- `in_memory` — a correct, IO-free backend for tests, benchmarks, and ephemeral
  runs, constructed with its specs (`InMemoryBackend::new(&specs)`); every commit
  is kept at once, and a reader is an O(1) snapshot of persistent maps. It also ships a latency-injecting wrapper for
  measuring commit cost.
- `conformance` — the suite every backend runs: a backend crate implements
  `conformance::Subject` once, runs `contract` as a plain test and `history`
  under proptest (`PROPTEST_CASES=1000` for a heavy run).

The on-disk backend lives in its own adapter crate, `zaino-persistence-lsm-tree`.
