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

## Code-review checklist: bugs Rust won't catch

Memory-safety guarantees and the type system catch a lot, but the
following bug classes pass the borrow checker cleanly and still ship as
real vulnerabilities or correctness failures. Audit every PR touching
filesystem, parsing, RPC boundaries, or external-input handling against
this list. Distilled from
<https://corrode.dev/blog/bugs-rust-wont-catch/>.

### 1. TOCTOU on filesystem paths

Operating on the same `&Path` across multiple syscalls lets an attacker
swap a symlink between the check and the use, redirecting privileged
operations to unintended targets.

- **Look for**: any `&Path` variable consumed by two or more syscalls
  (e.g. `fs::metadata(p)` then `File::open(p)`, or `fs::create_dir(p)`
  then `fs::set_permissions(p, ..)`); `fs::remove_file` or
  `set_permissions` after creation.
- **Mitigate**: prefer `OpenOptions::create_new(true)` (refuses
  pre-existing or symlinked targets); anchor follow-up operations on
  the file descriptor returned by the open, not on the path; use the
  `*at` family of syscalls relative to an open directory handle.

### 2. Delayed permission setting

Two-step "create then chmod" leaves a window where the file/dir exists
with default (often world-readable) permissions before the restrictive
mode lands.

- **Look for**: `File::create` / `fs::create_dir(_all)` immediately
  followed by `fs::set_permissions` or `chmod`; tests/fixtures where
  permissions are tightened after construction.
- **Mitigate**: set the mode atomically at creation —
  `OpenOptions::mode(0o600).create_new(true).open(..)` and
  `DirBuilderExt::mode(..)`. If process-wide control is needed, set
  `umask` explicitly at startup.

### 3. Path equality compared as strings

`==` on `Path`/`PathBuf` is a *string* comparison. It does not resolve
`./`, `..`, symlinks, or case-folded equivalents — two distinct strings
can name the same inode.

- **Look for**: `path == Path::new("..")`, `path.starts_with("/safe")`,
  or any security/authorization decision made on a path string without
  prior resolution.
- **Mitigate**: `fs::canonicalize` both sides before comparing; for
  arbitrary or non-canonicalizable paths, compare `(dev, inode)` pairs
  via `fs::metadata`.

### 4. Silent UTF-8 corruption of binary streams

`String::from_utf8_lossy` silently rewrites invalid bytes as U+FFFD;
round-tripping binary data through `String` (or printing it via
`print!`/`Display`) corrupts content.

- **Look for**: `from_utf8_lossy` on file/network input that may not be
  text; `print!("{}", buf)` on `Vec<u8>` or other byte buffers;
  `Display`/`{}` formatting applied to wire/protocol bytes.
- **Mitigate**: stay in `Vec<u8>` / `&[u8]` end-to-end. Use
  `Write::write_all` for binary output. Convert to `String` only at
  trust-validated text boundaries.

### 5. Panics on untrusted input → DoS

`unwrap()`, `expect()`, slice indexing, integer arithmetic without
`checked_*`, and parsing helpers like `from_utf8().expect()` all panic
on adversarial input, taking the whole process down.

- **Look for**: anything in this file's "No `.unwrap()`" section
  applied to data that crossed a trust boundary; `[i]` and `[..n]`
  slicing on input-derived indices; `as u32` / `+` / `*` on
  attacker-controlled sizes.
- **Mitigate**: propagate with `?`; use `.get(i)`, `.checked_add`,
  `usize::try_from`, etc.; turn on the relevant clippy lints
  (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`,
  `arithmetic_side_effects`) at module scope where feasible.

### 6. Discarded `Result` values

Throwing a `Result` away with `.ok()`, `let _ = ...`, or
`unwrap_or_default()` collapses the error into success and leaves
downstream code operating on stale or invalid state.

- **Look for**: `.ok();` at end of statement, `let _ = some_call();`
  without a justifying comment, `unwrap_or_default()` where default is
  semantically meaningful, loops that return only the *last* error
  encountered.
- **Mitigate**: propagate or aggregate. In a loop that may have
  multiple failures, track the worst exit code / first error and
  return at the end. Any deliberate discard requires an inline comment
  explaining why losing the error is safe.

### 7. Behaviour drift from a contract this code mirrors

Where zaino mirrors an external interface (zcashd / zebrad RPC, lwd
gRPC, exit codes of bundled CLIs), users script against the original's
quirks. Subtle semantic differences — flag interpretation, exit code,
error message text, edge-case return — break callers silently.

- **Look for**: new RPC handlers / endpoints whose behaviour is
  derived from reading the spec rather than from a passing
  compatibility test; `match`es on input flags whose mapping wasn't
  cross-checked against the upstream source; novel error variants /
  status codes returned where upstream returns something specific.
- **Mitigate**: pin the contract with a test that runs against the
  upstream tool (or a recorded golden response) — not with
  hand-written assertions about what we think the contract is.
  Bug-for-bug compatibility on edge cases is a feature.

### 8. Inputs resolved on the wrong side of a trust boundary

When user-controlled inputs are looked up via dynamically-loaded
machinery (NSS modules, plugin systems, dlopen-style backends) *after*
the process enters a more-trusted or more-restricted context, the
attacker controls the lookup path.

- **Look for**: any `chroot`, `setuid`, namespace transition, or
  switch into a sandboxed context that is followed by a name
  resolution call (user/group lookup, hostname resolution,
  configuration load, plugin discovery); `dlopen` or dynamic backend
  selection performed late.
- **Mitigate**: resolve all names and load all dynamic backends
  *before* the boundary transition. Static linking does not help if
  the resolver itself is dynamic (e.g. glibc NSS).

### How to apply this checklist

- On any PR that adds or modifies code touching the filesystem, byte
  streams, parsing of external input, RPC handlers, or process
  privilege/sandbox transitions, walk this list explicitly. The cost
  is a minute per PR; the cost of one of these landing is much higher.
- When you spot a category-(N) site already in tree without a fix,
  open a tracking issue rather than silently shipping the audit
  fix — the bug is older than your PR and may have caller-side
  expectations that need to change in lockstep.
