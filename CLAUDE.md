# Zaino AI Contributor Guidelines

## Tool selection

Always prefer Rust-native tools in domains where they are designed to operate.
Dependency and manifest changes go through `cargo add` / `cargo remove` /
`cargo update`. Code navigation and refactors go through rust-analyzer (see
the LSP section below). Verification goes through `cargo check` /
`cargo clippy` / `cargo fmt` / `cargo nextest`. Do not reach for Python, sed,
or regex sweeps over Rust source or `Cargo.toml` when a Rust tool covers the
job.

## Visibility: minimum required scope

All items (functions, methods, structs, enums, fields, modules) MUST use the
most restrictive visibility that compiles. Start with no visibility qualifier
(private). Only widen when the compiler rejects it, and then use the narrowest
scope that works:

1. `(private)` — default, no qualifier
1. `pub(super)` — visible to the parent module
1. `pub(crate)` — visible within the crate
1. `pub` — visible to external consumers

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
1. `#[tokio::test]` (current-thread) — the test body actually uses `.await`.
1. `#[tokio::test(flavor = "multi_thread")]` — the test genuinely requires
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

## Persistence-boundary conversions: `PersistentRecord`, not `From`/`TryFrom`

Every on-disk record is an explicit DTO — named `Persistent<X>` by
convention — that implements `zaino_persistence_codec::PersistentRecord`:

- `fn from_domain(domain: &X) -> Self` — infallible; the domain value is
  already valid.
- `fn into_domain(self) -> Result<X, DecodeError>` — the disk → domain
  validation step.
- The byte layout is the `RecordLayout` supertrait: `#[derive(PersistentRecord)]`
  for positional layouts, hand-written only for irregular framing (unframed
  repetition, count-prefixed nested collections).

Never `impl From` / `impl TryFrom` across this boundary.

**Why this rule exists**:

1. The `PersistentX → X` direction *is* the validation step for bytes
   coming off disk. A named method puts that contract in the API; a
   `TryFrom` leaves it implicit.
1. Named methods are grep-friendly and disambiguate direction at every
   call site (`record.into_domain()` reads direction and boundary; `.into()`
   hides both).
1. Splitting mapping (`from_domain`/`into_domain`) from layout
   (`RecordLayout`) keeps the error-prone byte half mechanical.

**Reference**: `PersistentHeaderValue` in
`packages/zaino-indexes/src/indexes/headers.rs`. Copy its shape, including
the pinned golden-bytes test beside it.

**Scope**: this rule covers DB-boundary conversions. It does not govern
conversions between two business-layer types, error `From` impls used
with `?`, or conversions involving foreign types that don't cross the
persistence or wire boundaries.

## Wire-boundary conversions: adapter-owned, not `From`/`TryFrom`

The same reasoning applies at the gRPC/JSON-RPC boundary — the wire →
domain direction is the *external-input* validation step. Domain types live
in `zaino-core` / `zaino-primitives`, which must never depend on a wire
schema (`zaino-proto`, jsonrpsee): that would recouple the domain to a
transport. So the serve **adapter** owns conversion:

- `to_wire` / `try_from_wire` live in the adapter crate, on a **local
  extension trait** (`trait ToWire { fn to_wire(self) -> ...; }`, impl'd
  for the foreign domain type) or as free functions. The orphan rule forbids
  inherent methods on a foreign type anyway.
- `try_from_wire` returns a per-conversion error enum documenting each
  rejection reason.
- Two adapters over one port render the same domain answer into two
  different wire shapes; neither leaks into the domain crate.

**Reference**: `ToWire` in `packages/zaino-lightserve/src/wire.rs`.

**Enforcement (covers both boundaries)**:

- CI lint: `makers lint-boundary-conversions` (run as part of
  `makers lint`) greps the tree for any `impl From` / `impl TryFrom`
  where either side is a `Persistent*` type or a `proto::` type and
  fails the build.
- Review checklist — apply on every PR that adds or changes a
  `Persistent*` record or a wire conversion:
  1. No `From`/`TryFrom` across either boundary. (The lint catches these,
     but read for them anyway.)
  1. Persistence goes through `PersistentRecord` (`from_domain` /
     `into_domain`); wire through the adapter's `to_wire` / `try_from_wire`.
  1. Visibility is the minimum that compiles (`EntryCodec`'s associated
     record types force `pub` on index DTOs; adapter wire traits stay
     `pub(crate)`).
  1. `Persistent*` types do *nothing else* — no business logic, no
     accessors. A golden-bytes / round-trip test sits next to each record
     and each wire conversion, not in a distant test module.

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
1. The message names the invariant being asserted, so a panic message
   is self-describing (e.g. `.expect("db_handler mutex poisoned")`, not
   `.expect("unwrap")`).
1. Propagation via `?` or a typed error is not cleaner at the call
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

## Crate usage guides: keep them current

Each crate is documented by a usage guide at `packages/<crate>/usage.md`, indexed
from the root `README.md`'s "Crate usage guides" section. The set is deliberately
incomplete and filled in crate by crate.

When a change adds or alters a crate's **public capability** — a new port,
service, config knob, public method, or a behavioural contract consumers rely on
— update that crate's guide: fold it into the relevant existing section, or add
one if none fits. If the crate has no guide yet, create it and add it to the
README index. This mirrors the CHANGELOG discipline: the CHANGELOG records *what
changed*, the guide records *how to use it*, and neither substitutes for the
other. Purely internal refactors need no guide edit.
