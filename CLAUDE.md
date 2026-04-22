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

**Scope**: this rule covers DB-boundary conversions only. It does not
govern conversions between two business-layer types, error `From` impls
used with `?`, conversions involving foreign types outside the
persistence boundary, or wire-format (gRPC) conversions.

**Enforcement**:

- CI lint: `makers lint-persistent-conversions` (run as part of
  `makers lint`) greps the tree for any `impl From` / `impl TryFrom`
  where either side of the boundary is a `Persistent*` type and fails
  the build. Mechanically prevents the common drift.
- Review checklist — apply on every PR that touches `types/db/` or
  introduces a new `Persistent*` type:
  1. No `impl From<&X> for PersistentY` or `impl From<PersistentX> for Y`
     (the lint catches this, but read for it anyway).
  2. Conversion methods are named literally `from_business` /
     `into_business` (or a fallible `into_business*` variant). Any
     deviation has an in-file comment explaining why.
  3. The persistent type's visibility is `pub(super)` unless the
     compiler required a wider scope. Don't widen speculatively.
  4. The persistent type does *nothing else* — no business logic, no
     accessors — it only crosses the serde boundary. The round-trip
     test for the pair lives in the same file under
     `#[cfg(test)] mod tests`.
