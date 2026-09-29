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

## Disk and wire boundaries: named functions, not `From`/`TryFrom`

Bytes coming off disk or off the wire are the input-validation step, so the
conversion is a named function whose signature states the direction and the
failure — never `impl From` / `impl TryFrom`, which hide both behind `.into()`.

- Disk: the index that owns a file owns its layout, as `encode(&X) -> [u8; N]`
  and `decode(&[u8; N]) -> X` (or `Result<X, E>` when some bytes are invalid).
  Reference: `packages/zaino-index-tree-state/src/heights.rs`.
- Wire: domain crates (`zaino-primitives`, …) never depend on `zaino-proto`.
  The crate that produces or consumes the wire bytes owns the conversion (e.g.
  `encode_compact_block` in `packages/zaino-index-compact-block/src/build.rs`); a fallible
  wire → domain conversion returns an error enum naming each rejection.
- A golden-bytes / round-trip test sits next to each layout and each wire
  conversion, not in a distant test module.

Error `From` impls used with `?`, and conversions between two domain types,
are outside this rule.

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
from the root `README.md`'s "Crate usage guides" section.

When a change adds or alters a crate's **public capability** — a new port,
service, config knob, public method, or a behavioural contract consumers rely on
— update that crate's guide: fold it into the relevant existing section, or add
one if none fits. If the crate has no guide yet, create it and add it to the
README index. This mirrors the CHANGELOG discipline: the CHANGELOG records *what
changed*, the guide records *how to use it*, and neither substitutes for the
other. Purely internal refactors need no guide edit.

## `zaino-persistence` changes: heavy proptest run before you're done

Every index's durability rests on `packages/zaino-persistence`. After **any**
change inside it (code, tests, `SimFs`, even a refactor), and before reporting
the work as done, run the heavy validation below. The default `cargo test` run
is deliberately light (64 model-test cases, ~2 s) and is not enough.

```bash
# 1. heavy: fresh random seeds, round after round, for at least 3 minutes
end=$((SECONDS + 180)); round=0
while [ $SECONDS -lt $end ]; do
  round=$((round + 1))
  PROPTEST_CASES=1000 cargo test -p zaino-persistence --features testing \
    || { echo "FAILED in round $round"; break; }
done

# 2. every crate built on it
cargo test -p zaino-internal-block-hash-to-height -p zaino-internal-value-balance \
  -p zaino-index-transparent-address -p zaino-index-compact-block -p zaino-index-tree-state
```

- Step 1 re-runs the whole persistence suite each round, so the thread-timing
  sensitive tests (background merges, failed I/O, crash states) repeat too, and
  the model test (`random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss`)
  runs 1000 fresh random histories per round.
- Env overrides (proptest 1.6): `PROPTEST_CASES` (cases per proptest; overrides
  the in-code 64), `PROPTEST_MAX_SHRINK_ITERS` (shrink harder on a failure),
  `PROPTEST_VERBOSE=1` (print each case). Lengthen the run by raising the `180`
  (seconds); never shorten it.
- Keep debug assertions on: use the default test profile, not `--release`.
  Some invariant checks (`Pages::read_unchecked`) are `debug_assert!`.
- A failure writes the shrunk case to
  `packages/zaino-persistence/proptest-regressions/`. Commit that file (it
  replays on every future run), fix the bug, and rerun the whole block. Never
  lower case counts, loosen an assertion, or skip a test to get green.
- Report the rounds completed and the result in your summary.
