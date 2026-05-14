# Zaino AI Contributor Guidelines

## Visibility: minimum required scope

All items (functions, methods, structs, enums, fields, modules) MUST use the
most restrictive visibility that compiles. Start with no visibility qualifier
(private). Only widen when the compiler rejects it, and then use the narrowest
scope that works:

1. `(private)` — default, no qualifier
2. `pub(super)` — visible to the parent module
3. `pub(crate)` — visible within the crate
4. `pub` — visible to external consumers

Never preemptively make something `pub`. If a test needs access to an internal,
prefer `pub(crate)` or a `#[cfg(test)]` helper over `pub`. If an item is only
used within its own module, it stays private even if "it might be useful later."

## DRY: deduplicate with functions first

Always produce the DRYest implementation possible. When eliminating
duplication, prefer plain functions (`fn`) over macros or other patterns.
Resort to macros only when `fn` cannot express the abstraction (e.g. the
call site requires a string literal, or the pattern spans syntactic
constructs that functions cannot capture).

## Test attributes: minimum justified complexity

Every test starts at `#[test]`. Escalate only when the test body demands
it, and pick the narrowest escalation that works:

1. `#[test]` — default. Synchronous tests.
2. `#[tokio::test]` (current-thread) — the test body actually uses `.await`.
3. `#[tokio::test(flavor = "multi_thread")]` — the test genuinely requires
   multiple OS threads (a race under test, `spawn_blocking` that must run
   concurrently with the test future, code that would deadlock on
   current-thread).

Never inherit a heavier attribute from a neighbouring test by convention —
each test is justified on its own body. `multi_thread` is not a free
upgrade: it adds runtime-startup cost, introduces scheduling
nondeterminism, and can mask bugs that would surface on current-thread.

When auditing or adding a test, verify the justification by reading the
body: is there any `.await`? Any task spawn? Any reliance on real
timers? If not, downgrade. Leave a brief comment only if the choice is
non-obvious (e.g. "multi_thread required: test exercises a race between
writer and reader on the db").

## Persistence-boundary conversions: named methods, not `From`/`TryFrom`

Every DB-boundary helper that mirrors a business-layer type — named
`Persistent<X>` by convention — crosses its boundary through inherent
methods, not `impl From` / `impl TryFrom`. The canonical pair:

- `impl PersistentX { pub(super) fn from_business(src: &X) -> Self }`
  (replaces `impl From<&X> for PersistentX`)
- `impl PersistentX { pub(super) fn into_business(self) -> X }`
  (replaces `impl From<PersistentX> for X`; return `Result<X, ..>` if
  the on-disk → business step can fail validation)

Both methods live on the persistent type. Visibility is `pub(super)` —
`PersistentX` is module-private-by-design; only its sibling consumers
in the same directory need access.

**Why this rule exists**:

1. The `PersistentX → X` direction *is* the validation step for bytes
   coming off disk. A named method puts that contract in the API; a
   `TryFrom` leaves it implicit.
2. `TryFrom` forces one `Error` type per impl; separate methods give
   per-conversion error granularity.
3. Named methods are grep-friendly and disambiguate direction at every
   call site (`pbc.into_business()` reads direction and boundary; `.into()`
   hides both).

**Reference**: `PersistentBlockContext` in
`packages/zaino-state/src/chain_index/types/db/block.rs`. Copy its shape
when adding new `Persistent*` types.

**Scope**: this rule covers DB-boundary conversions. It does not govern
conversions between two business-layer types, error `From` impls used
with `?`, or conversions involving foreign types that don't cross the
persistence or wire boundaries.

## Wire-boundary conversions: named methods, not `From`/`TryFrom`

The same rule applies at the gRPC/wire boundary for the same reasons —
the wire → business direction is the *external-input* validation step
and the named method encodes that contract in the API surface. Canonical
methods live on the business-layer type (proto types are foreign; we
can't add inherent methods to them):

- `impl X { pub fn to_wire(&self) -> proto::X }` — infallible forward.
  Replaces `impl From<X> for proto::X`.
- `impl X { pub fn try_from_wire(w: proto::X) -> Result<Self, WireXError> }`
  — fallible reverse. The conversion *is* the wire-input validation
  step; the `WireXError` enum documents each rejection reason.
  Replaces `impl TryFrom<proto::X> for X`.

**Reference**: `BlockIndex` wire methods in
`packages/zaino-state/src/chain_index/types/wire.rs`. Copy its shape
when adding wire conversions for other business types (BlockHash,
TransactionHash, etc.).

**Enforcement (covers both boundaries)**:

- CI lint: `makers lint-boundary-conversions` (run as part of
  `makers lint`) greps the tree for any `impl From` / `impl TryFrom`
  where either side is a `Persistent*` type or a `proto::` type and
  fails the build. Mechanically prevents the common drift at both
  boundaries.
- Review checklist — apply on every PR that touches `types/db/`,
  `types/wire.rs`, or introduces a new `Persistent*` type or wire
  conversion:
  1. No `impl From<&X> for PersistentY` / `impl From<PersistentX> for Y`;
     no `impl From<X> for proto::Y` / `impl TryFrom<proto::X> for Y`.
     (The lint catches these, but read for them anyway.)
  2. Persistence methods are named `from_business` / `into_business`
     (fallible variants `into_business*`). Wire methods are named
     `to_wire` / `try_from_wire`. Any deviation has an in-file comment
     explaining why.
  3. `Persistent*` types are `pub(super)`. Wire methods are `pub`
     (they're part of the business type's public API). Don't widen
     `Persistent*` speculatively.
  4. `Persistent*` types do *nothing else* — no business logic, no
     accessors — they only cross the serde boundary. Round-trip tests
     for the pair live in the same file under `#[cfg(test)] mod tests`.
     Wire conversions get the same treatment: a golden / round-trip
     test next to the method, not in a distant test module.

## No `.unwrap()`: propagate or handle every error

`.unwrap()` is DISALLOWED in all production code without exception.
Propagate errors with `?`, return a typed error, or handle the
`None`/`Err` case explicitly. If a value is truly infallible, prefer
expressing that in the type system (e.g. via `NonZeroU32`, a checked
constructor, or an exhaustive `match`) over asserting it at runtime.

`.expect("...")` is allowed in production code only under these
constraints:

1. The failure represents a genuine program invariant that cannot be
   encoded in the type system or recovered from at runtime (e.g. a
   `Mutex` that is only ever held for a non-panicking swap, so
   `PoisonError` indicates an already-crashed thread).
2. The message names the invariant being asserted, so a panic message
   is self-describing (e.g. `.expect("db_handler mutex poisoned")`, not
   `.expect("unwrap")`).
3. Propagation via `?` or a typed error is not cleaner at the call
   site. If the surrounding function already returns a `Result`, prefer
   `?`.

When in doubt, propagate. Reach for `.expect(...)` only when the
alternative is materially worse.

In test code `.unwrap()` is tolerated but not encouraged: before using
it, double-check whether `?` (in a `fn() -> Result<_, _>` test), a more
descriptive `.expect("...")` with a message naming the invariant, or an
`assert!`/`assert_matches!` would make the failure mode clearer. Prefer
those alternatives whenever they fit.

## LMDB terminology: use the canonical names

Zaino's persistence layer is built on LMDB (Lightning Memory-Mapped
Database) via the `lmdb` crate. Every component, type, symbol, or term
that names an LMDB concept MUST use LMDB's canonical vocabulary —
generic database synonyms ("store", "table", "session", "view") are
DISALLOWED where an exact LMDB term applies. When a Zaino abstraction
("ZainoDB") composes LMDB primitives into something LMDB does not
natively express, the type/module MUST carry a doc comment that names
the LMDB primitives it is built from.

### Canonical vocabulary

Use the **LMDB term** column. The **synonyms** column lists names that
MUST NOT appear in Zaino code, doc comments, or design docs when the
exact LMDB term applies.

| Concept                                  | LMDB term (use this)                                               | Disallowed synonyms                          |
|------------------------------------------|--------------------------------------------------------------------|----------------------------------------------|
| Top-level handle / memory-mapped file    | **environment** (`lmdb::Environment`, C: `MDB_env`)                | "store", "db file", "db instance"            |
| Named B+tree within an environment       | **database** (`lmdb::Database`, C: `MDB_dbi`; a.k.a. "named DB")   | "table", "collection", "namespace", "tree"   |
| Read txn / write txn handle              | **transaction**, qualified **read txn** / **write txn**            | "session", "view-handle", "connection"       |
| MVCC view a read txn observes            | **snapshot** (the LMDB term for the read-only view)                | "version", "checkpoint"                      |
| Key-ordered iterator scoped to a txn     | **cursor** (`lmdb::Cursor`, C: `MDB_cursor`)                       | "iterator", "scanner", "walker"              |
| Byte-slice operands                      | **key** / **value** (C: `MDB_val`)                                 | "id" / "record" (when discussing the slice)  |
| On-disk allocation unit                  | **page**                                                           | "block" (collides with Zcash "block"), "chunk" |
| Multi-value-per-key flag                 | **DUPSORT**                                                        | "multimap", "secondary index"                |
| Reader registration table & its entries  | **reader table** / **reader slot**                                 | "session table"                              |
| Basic operations                         | **put** / **get** / **del**                                        | "insert" / "lookup" / "remove"               |
| Cursor positioning                       | **MDB_FIRST / NEXT / PREV / SET / SET_RANGE / …**                  | ad-hoc names                                 |

Note on "block": Zcash uses "block" for a chain block. Always say
**page** for the LMDB allocation unit and **block** for the chain
entity — never overload the term.

### Doc-comment rule for non-native (ZainoDB) concepts

A "ZainoDB" concept is any Zaino abstraction that composes LMDB
primitives into something LMDB does not natively express
(e.g. a `Persistent<X>` wrapper, a multi-database atomic write, a
cross-database lookup that holds one read txn open across two cursors).
Every such item MUST carry a doc comment whose body explicitly names
the LMDB primitives in play. Example shape:

```rust
/// Atomically advance the chain tip across the `blocks` and
/// `headers` databases.
///
/// **LMDB shape**: opens one write **txn** on the **environment**,
/// `put`s into the `blocks` **database** (`MDB_dbi`) and the
/// `headers` **database**, then **commits**. Readers observe a
/// consistent **snapshot** (no torn-tip state) because both writes
/// share the same `MDB_txn`.
pub fn advance_tip(&self, ..) -> Result<..> { .. }
```

The doc comment lets a reader fluent in LMDB verify the implementation
matches its description without chasing call sites.

### Dual-naming rule

When prose reads more naturally with a loose database term *and* LMDB
has an exact term, write **both** — lead with the generic term and
parenthesize the LMDB term on first use in a file:

- "the chain-index store (LMDB **environment**)"
- "the per-block table (LMDB **database** `blocks`)"
- "open a read-only view (read **txn**, observing a **snapshot**)"
- "iterate (open a **cursor**) over the heights"

After the first parenthesized introduction in a file, subsequent
references in that file should use the LMDB term alone.

### Scope and exceptions

- **Applies to**: type names, field names, module names, function
  names, and local variable names in code that touches the persistence
  layer; doc comments throughout `zaino-state` and any other
  LMDB-backed module; in-tree design notes and PR descriptions
  discussing persistence.
- **Does not apply to** business-layer types whose names come from the
  Zcash protocol (`BlockHash`, `Transaction`, `BlockHeight`, etc.).
  These keep their domain names; the LMDB framing goes on the doc
  comment of the *persistence wrapper* (e.g. `PersistentBlockContext`),
  never on the business type.
- The `Persistent<X>` naming (see *Persistence-boundary conversions*)
  is preserved: `Persistent<X>` means "the on-disk encoded form of an
  `X`, ready to serve as a **value** (`MDB_val`) in an LMDB **put**".
  New `Persistent*` types MUST carry a doc comment saying so.
- Naming collision with Rust's `std::env`: refer to the LMDB
  environment as **environment** (full word) in prose; in code,
  `lmdb::Environment` / `env: &Environment` is canonical and short
  enough — never abbreviate to anything other than `env`.

### Review checklist (apply on every PR touching persistence)

1. Grep the diff for disallowed synonyms ("store", "table",
   "collection", "session", "view-handle") and rewrite each to its
   LMDB term, or justify in-file why the exact term doesn't apply.
2. Every new persistence type or function carries a doc comment
   naming the LMDB primitives it touches (environment / database /
   read txn / write txn / cursor / put / get / del / commit /
   snapshot / page / DUPSORT).
3. On first use within a file, prose that uses a generic DB term
   names the LMDB term in parentheses; later references use the LMDB
   term alone.
4. The Zcash "block" and the LMDB "page" are kept distinct — no
   prose says "block" to mean an LMDB page.
