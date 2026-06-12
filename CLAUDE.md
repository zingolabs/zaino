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

## Use the language server (LSP) for definitive code intelligence

When answering *where* a symbol is defined, *who* calls or references it,
its type, or its implementors, use the language server (go-to-definition,
find-references, hover, call-hierarchy, workspace-symbol) — not `grep` or
text search. Text search *guesses*; the LSP *resolves*: it follows `use`
aliases, re-exports, generics, trait impls, and macro expansions a regex
cannot, and it is not fooled by comments, strings, or shadowed names.
Reach for `grep` only as a fallback — when the server is genuinely
unavailable, still indexing, or the target isn't code it understands — and
say so when you do.

**Multi-workspace caveat (this repo):** the tree has three *separate* Cargo
workspaces — `Cargo.toml` (root, `packages/*` production), `integration-tests/Cargo.toml`
(walletless tests), and `integration-tests/wallet-tests/Cargo.toml` (wallet
tests). rust-analyzer is scoped (in `.helix/languages.toml`) to **one
integration-test workspace at a time** — indexing more than one wedges the
server, and the production workspace is intentionally never indexed (we only
need LSP on test code; production crates still resolve as path dependencies, so
go-to-def into them works). To switch which test workspace is analyzed, swap the
active `linkedProjects` entry in `.helix/languages.toml` (comment one, uncomment
the other) and reload the LSP (`:lsp-restart`). Because only one workspace is
loaded at a time, an empty LSP result usually means "the other workspace isn't
loaded," not "no references" — confirm which workspace is active first.
