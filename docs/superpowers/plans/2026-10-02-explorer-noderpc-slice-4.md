# Explorer node-RPC, slice 4 + chain info Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a real engine satisfy `NodeRpcService`, and serve the four
explorer methods that depend on full blocks, decoded transactions and chain
info.

**Architecture:** `BlockRead` and `TransactionRead` are the last two
`NodeRpcReads` members with no non-mock implementation, so no real engine can
satisfy `NodeRpcService` and the use case is undeployable. Both are
**passthrough** — the finalised store holds compact projections, not full block
bytes. Blocks pass through almost untouched because `zaino-source`'s
`get_block` already returns the domain `Block`; decoded transactions need one
new source port, because `get_transaction` returns bytes and the domain
`Transaction` conversion lives in the validator adapter, where `zaino-core`
cannot reach it.

**Tech Stack:** Rust 2021, `zaino-source` port traits, `zaino-convert-zebra`'s
`transaction_from_zebra`, `jsonrpsee`, `serde`, `tokio`.

**Spec:** `/home/chona/zingo/zingolabs/zaino-design/design/explorer-grant-completion.md`

**Why this slice is the gate.** Verified at `d3c91dd6f`:

```
BlockRead        → no non-mock impl anywhere in the workspace
TransactionRead  → no non-mock impl anywhere in the workspace
SpendRead / AddressRead / TreestateRead / ChainInfoRead / RawTransactionRead → zaino-core
```

Until the first two exist, the deployment + wiring work cannot compile a
node-rpc arm, so this slice precedes it.

## Global Constraints

- Branch `feat/noderpc-explorer`, worktree
  `/home/chona/zingo/zingolabs/zaino/noderpc-explorer`. All paths are relative
  to it.
- `unsafe_code = "forbid"`, `missing_docs = "warn"` in every crate. Every new
  public item needs a doc comment.
- **No `as` casts for numeric conversion.** `From`/`Into`/`TryFrom`/`checked_*`.
- **Minimum visibility that compiles.** Start private; widen only when the
  compiler rejects it.
- No `mod.rs`. A module with children is `foo.rs` + `foo/`.
- **`zaino-core` must not depend on `zaino-convert-zebra`, `zaino-source-zebra*`
  or any validator crate.** It is validator-agnostic by design, which is the
  whole reason the decoded-transaction conversion needs a port rather than an
  inline parse. Check `packages/zaino-core/Cargo.toml` after any dependency
  change; if a task seems to need one of those crates in core, stop and report.
- **Wildcard match arms:** `clippy::wildcard_enum_match_arm` is denied in
  `zaino-core`, `zaino-component`, `zaino-async`, `zaino-logging` — and
  `zaino-core` is edited by this slice, so it binds here for real. Elsewhere
  prefer one arm per variant anyway, because a catch-all silently absorbs a
  variant added later; but do not distort a design to dodge a lint that is not
  active.
- **Errors: typed causes, decided context by context.** Never `format!` a cause
  that has a type into a message. Pick the variant matching *this* site's
  meaning. `Fatal` is "unrecoverable backend failure" — a server fault, never a
  params error. Rendering to a string is correct only at the JSON-RPC wire
  boundary.
- **No `.unwrap()` in production code.** `.expect("...")` only for a genuine
  untypeable invariant, message naming it. Prefer `?`.
- **`HeightRange` is inclusive** — `[start, end]`, `end` is the highest height.
  Never add or subtract one from a bound. Note `zaino-core`'s `split_at_seam`
  is half-open on this branch and is fixed to inclusive on `feat/address-seam`
  (commit `83afd543d`); do not fix it here, and do not write code that depends
  on either reading — if a task needs the seam split, stop and report.
- **Every test must fail if the behaviour it names is removed.** Script
  fixtures; a test whose assertion holds for trivially empty or unscripted
  input proves nothing.
- Doc comments describe present code, self-contained. No "previously", "used
  to", or before/after narrative in a `///`.
- **Before changing any public signature, find its call sites workspace-wide:**
  `grep -rn "<name>" packages/ --include=*.rs`, including `tests/` directories,
  which are separate compilation targets. Add every owning crate to the gate.
- **Verification gate.** Verify per-crate, never `--workspace` (this host cannot
  link `aws-lc-sys`):

  ```
  cargo test -p zaino-service                     # must COMPILE with the feature OFF
  cargo test -p zaino-service --features testing
  cargo test -p zaino-source
  cargo test -p zaino-source-zebra-rpc
  cargo test -p zaino-core
  cargo test -p zaino-noderpc
  cargo test -p zaino-lightserve
  cargo test -p zaino-wallet
  cargo test -p zaino-runtime
  cargo clippy -p <each touched crate> --no-deps --all-targets -- -D warnings
  makers fmt
  ```

  `--no-deps` is required: without it clippy lints path dependencies and trips a
  pre-existing `manual_is_multiple_of` in `zaino-component`. Any task touching
  `zaino-service/src/testing.rs` must run the whole list — that mock is shared
  and has already broken a sibling crate once on this branch.
- **Baseline counts that must not regress:** zaino-address 11, zaino-noderpc 26,
  zaino-service 13 (`--features testing`) and compiling with it off,
  zaino-lightserve 26, zaino-wallet 2, zaino-core 30, zaino-runtime 35.
- Commit forward only — never `--amend`, never `git stash` (shared stack).
- Do NOT add a `Co-Authored-By` trailer or any attribution line.
- Update a crate's `usage.md` when the task adds public capability to a crate
  that has one. `zaino-noderpc` has none and is still a POC; leave it.

## Review Focus

Five input classes this slice will meet and that a naive implementation gets
wrong. Each has its test pinned to the task that owns the code.

1. **A height or hash the validator does not know must be `Ok(None)`, not an
   error.** `getblock` on a future height is an ordinary miss; the explorer's
   search box probes arbitrary strings. (Tasks 2, 3)
2. **A validator that cannot be reached must stay an error, never an empty
   success.** The explorer's warmers cache successes and ignore errors, so a
   transient rendered as `Ok` with defaults poisons their cache for 15 s.
   (Tasks 1, 2, 3, 4)
3. **`stream_blocks` over an inclusive range must include both endpoints** and
   must stop at the first failure rather than silently truncating. (Task 2)
4. **A txid in the mempool is not mined.** `transaction_status` must distinguish
   `Mined(h)` from `Unknown`, and `TransactionLocation::Mempool` is not
   `Orphaned`. (Task 3)
5. **`getblockchaininfo` is polled every 15 s and is the only source for four
   explorer views** — it must carry `blocks`, `difficulty`, `chain`,
   `valuePools`, `size_on_disk` and `commitments`, all of which
   `BlockchainInfo` already has. Dropping one silently blanks a page. (Task 4)

---

### Task 1: A source port for the decoded transaction

`get_transaction` returns `TransactionResponse { bytes, location }`. The domain
`Transaction` is produced by `zaino_convert_zebra::transaction_from_zebra`,
which lives in the validator adapter — and `zaino-core` must not depend on it.
So the decoded form needs its own port.

**Files:**
- Create: `packages/zaino-source/src/get_transaction_verbose.rs`
- Modify: `packages/zaino-source/src/lib.rs`
- Modify: `packages/zaino-source/src/mock.rs`
- Modify: `packages/zaino-source/src/arc_forward.rs`
- Modify: `packages/zaino-source-zebra-rpc/src/adapter.rs`
- Modify: `packages/zaino-source/usage.md`

**Interfaces:**
- Consumes: `zaino_primitives::types::{Transaction, TransactionId, TransactionLocation}`; `zaino_convert_zebra::transaction_from_zebra` (adapter side only).
- Produces: `OneShotGetTransactionVerbose::get_transaction_verbose(TransactionId) -> Result<DecodedTransaction, QueryError<GetTransactionVerboseError, _>>`, where `DecodedTransaction { transaction: Transaction, location: TransactionLocation }`.

- [ ] **Step 1: Read the two templates before writing anything**

Read `packages/zaino-source/src/get_transaction.rs` end to end — it is the port
this one sits beside, and it shows the `#[zaino_source_macros::resilient_port]`
attribute, the `*Error` enum shape, and the doc-comment voice. Then read its
implementation at `packages/zaino-source-zebra-rpc/src/adapter.rs:1014`, which
is the template for Step 5. Report in one line what the existing error enum's
variants are, so the new one mirrors them rather than inventing a shape.

- [ ] **Step 2: Write the port**

Create `packages/zaino-source/src/get_transaction_verbose.rs`. Mirror
`get_transaction.rs`'s structure exactly — same attribute, same error-enum
shape, same doc voice. The response type pairs the decoded transaction with
where it lives, because a caller needs both and fetching them separately would
let them disagree:

```rust
/// A transaction decoded into its pool structure, with where it lives.
///
/// Paired because a caller needs both and two fetches could disagree: the
/// transaction could be mined between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTransaction {
    /// The transaction, decomposed by pool.
    pub transaction: Transaction,
    /// Where the transaction was found.
    pub location: TransactionLocation,
}
```

The domain error carries the same "not found" case as `GetTransactionError`.
Name the trait `OneShotGetTransactionVerbose` and the method
`get_transaction_verbose`, matching the file. Document *why* this exists
separately from `get_transaction`: the raw form is what a wallet parses itself,
and the decoded form is the explorer surface whose conversion needs the
validator's chain library.

- [ ] **Step 3: Export it**

Add `mod get_transaction_verbose;` and the `pub use` to
`packages/zaino-source/src/lib.rs`, beside `get_transaction`'s. Match whatever
form the neighbours use.

- [ ] **Step 4: Forward it and mock it**

`packages/zaino-source/src/arc_forward.rs` forwards every port through an
`Arc`; add the new one beside `get_transaction` (around line 194). Then extend
`packages/zaino-source/src/mock.rs`: add a scriptable field and a builder
method mirroring `respond_transaction` (line 61), and implement the port.

- [ ] **Step 5: Write the failing adapter test, then implement it**

In `packages/zaino-source-zebra-rpc/`, add a test that a known raw transaction
decodes to the expected domain `Transaction`, then implement the port on the
adapter beside `get_transaction` (`adapter.rs:1014`) using
`zaino_convert_zebra::transaction_from_zebra`. The adapter already deserializes
raw bytes with Zebra's own deserializer for other ports — find that helper and
reuse it rather than adding a second deserialization path; say in your report
which one you used.

Review Focus 2 applies: a transport failure must surface as a non-domain error,
never as a decoded-but-empty transaction.

- [ ] **Step 6: Run the gate and commit**

```
cargo test -p zaino-source
cargo test -p zaino-source-zebra-rpc
cargo clippy -p zaino-source -p zaino-source-zebra-rpc --no-deps --all-targets -- -D warnings
makers fmt
```

Update `packages/zaino-source/usage.md` for the new port, then:

```bash
git add packages/zaino-source packages/zaino-source-zebra-rpc
git commit -m "feat(source): a port for the decoded transaction

get_transaction returns the consensus bytes, which is what a wallet wants --
it parses them itself. An explorer wants the transaction decomposed by pool,
and that conversion needs the validator's chain library, which zaino-core
cannot depend on without becoming validator-specific.

So the decoded form is its own port, paired with its location because two
fetches could disagree: the transaction could be mined between them."
```

---

### Task 2: `BlockRead` on the real engine

**Files:**
- Create: `packages/zaino-core/src/engine/block.rs`
- Modify: `packages/zaino-core/src/engine.rs`
- Modify: `packages/zaino-core/src/passthrough.rs`
- Modify: `packages/zaino-core/src/tests.rs`
- Modify: `packages/zaino-core/usage.md`

**Interfaces:**
- Consumes: `zaino_source::{GetBlock, GetBlockByHash}`; `ChainSegment::pinned_tip`.
- Produces: `impl BlockRead for EngineSnapshot<F, N, Src, R>`, and `PassthroughProvider::{block, block_by_hash}`.

- [ ] **Step 1: Write the failing tests**

Add to `packages/zaino-core/src/tests.rs`, following its existing helpers (it
builds engines with `ValidatorClient::new(MockChain::new(), RetryPolicy::default())`
— read lines 30–60 first). Cover, each with a scripted fixture:

- a block fetched by height returns the scripted block
- a block fetched by hash returns the same block
- **Review Focus 1:** an unknown height answers `Ok(None)`, not an error
- **Review Focus 3:** `stream_blocks` over an inclusive `[a, b]` yields `b - a + 1` blocks, including both endpoints
- **Review Focus 2:** an unreachable validator errors rather than answering `Ok(None)`. Note `ValidatorClient` **retries**, so `fail_next(1, ..)` can succeed on attempt 2 — fail more attempts than the policy allows (the existing suite uses `fail_next(3, FailureMode::Connection)`) or build a single-attempt policy. `FailureMode` has no `Unavailable` variant; the variants are `Connection`, `Timeout`, `Parse`, `RpcError(i32)`.
- `tip` returns the pinned tip, and errors when there is none

- [ ] **Step 2: Run them and watch them fail**

`cargo test -p zaino-core block` — expect method-not-found on `BlockRead`.

- [ ] **Step 3: Add the passthrough methods**

In `packages/zaino-core/src/passthrough.rs`, add `block` and `block_by_hash`
following the file's established shape exactly (read the `treestate` and
`raw_transaction` impls first). Each maps the port's domain "not found" to
`Ok(None)` and both transport arms to the read error's transient variant —
**with the cause preserved, not formatted into a string** where the error type
allows it. `BlockReadError` is generated by the `read_error!` macro and its
variants carry `String`, so a `format!` there is unavoidable; say so in your
report rather than pretending otherwise.

- [ ] **Step 4: Implement the trait**

Create `packages/zaino-core/src/engine/block.rs` — a new module rather than
more sections in `snapshot.rs`, matching how `address.rs`, `spend.rs` and
`treestate.rs` already each own one read. Implement `BlockRead` for
`EngineSnapshot`:

- `tip` — from `self.local().pinned_tip()`, erroring when absent. **Local, not
  passthrough:** the pinned tip is what this snapshot is coherent against, and
  asking the validator would return a tip the rest of the snapshot does not
  share.
- `block` — match `BlockSelector::{Height, Hash}` onto the two passthrough
  methods. One arm per variant; the lint is active in this crate.
- `block_header` — the header of the block the same selector names. `Block`
  carries its `header`, so this needs no separate port.
- `block_height` — the height of the block with that hash, from its header.
- `stream_blocks` — the blocks of an inclusive range in ascending order.
  `HeightRange` is inclusive, so the last height is included. Stop at the first
  error rather than skipping it; a consumer must not mistake a truncated stream
  for a complete one.

Declare the module in `packages/zaino-core/src/engine.rs` beside its siblings.

- [ ] **Step 5: Run the gate and commit**

The full gate, because `zaino-core` is widely depended on. Then update
`packages/zaino-core/usage.md` and commit:

```bash
git add packages/zaino-core
git commit -m "feat(core): the engine answers block reads

Always passthrough: the finalised store holds compact projections, not full
block bytes, so a full block can only come from the validator. The source port
already returns the domain Block, so this routes a BlockSelector to the
by-height or by-hash port and reads the header off the block it already has.

The tip is the exception and stays local: it is what the snapshot is coherent
against, and the validator's tip is one the rest of the snapshot does not
share.

An unknown height or hash is Ok(None) -- an ordinary miss, since callers probe
arbitrary input -- while an unreachable validator stays an error, so a consumer
that caches successes cannot cache a miss that was really a failure."
```

---

### Task 3: `TransactionRead` on the real engine

**Files:**
- Create: `packages/zaino-core/src/engine/transaction.rs`
- Modify: `packages/zaino-core/src/engine.rs`
- Modify: `packages/zaino-core/src/passthrough.rs`
- Modify: `packages/zaino-core/src/tests.rs`
- Modify: `packages/zaino-core/usage.md`

**Interfaces:**
- Consumes: Task 1's `zaino_source::OneShotGetTransactionVerbose` (bind the canonical resilient trait, not the `OneShot*` one — `PassthroughProvider` binds the twins).
- Produces: `impl TransactionRead for EngineSnapshot<..>`, `PassthroughProvider::transaction`.

- [ ] **Step 1: Write the failing tests**

In `packages/zaino-core/src/tests.rs`:

- a scripted txid returns the decoded transaction
- **Review Focus 1:** an unknown txid answers `Ok(None)`
- **Review Focus 4:** `transaction_status` maps `TransactionLocation::BestChain(h)` to `TxStatus::Mined(h)`, `NonBestChain` to `TxStatus::Orphaned`, and `Mempool` to `TxStatus::Unknown` — a mempool transaction is not mined and is **not** orphaned. Assert all three; a test covering only the mined case would miss the distinction the enum exists to make.
- **Review Focus 2:** an unreachable validator errors rather than answering `Ok(None)`, with the retry caveat from Task 2 Step 1.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Add the passthrough method and implement the trait**

`PassthroughProvider::transaction` over the new port, same shape as its
neighbours. Then `packages/zaino-core/src/engine/transaction.rs`:

- `transaction` — passthrough, returning the decoded form.
- `transaction_status` — from the same fetch's `location`. An absent
  transaction is `TxStatus::Unknown`.

`TxStatus` has exactly three variants (`Mined(Height)`, `Orphaned`, `Unknown`)
and `TransactionLocation` exactly three (`BestChain(Height)`, `NonBestChain`,
`Mempool`); write one arm per variant, no catch-all — the lint is active here.

- [ ] **Step 4: Run the gate and commit**

```bash
git add packages/zaino-core
git commit -m "feat(core): the engine answers decoded transaction reads

Passthrough over the decoded-transaction port: the store holds no transaction
bytes, and the pool decomposition needs the validator's chain library.

transaction_status reads the location the same fetch returned, so the status
and the transaction cannot disagree. A mempool transaction is Unknown rather
than Orphaned -- it is not mined and it has not been reorged out, and
collapsing those two is how a consumer concludes a pending transaction failed.

With this and BlockRead, EngineSnapshot satisfies NodeRpcReads, so a real
engine satisfies NodeRpcService and the node-rpc use case becomes deployable."
```

---

### Task 4: Widen `ChainInfoRead` to the real chain info

`ChainInfo` is currently two fields (`tip`, `estimated_height`) and the adapter
renders `"estimated_height=N"` — a stub, not JSON. The explorer polls
`getblockchaininfo` every 15 s and it is the only source for its block-count,
difficulty, blockchain-size and Orchard-pool views.

`zaino_primitives::types::BlockchainInfo` already carries everything needed:
`chain`, `blocks`, `headers`, `estimated_height`, `best_block_hash`,
`difficulty`, `verification_progress`, `size_on_disk`, `commitments`,
`value_pools`. So this is passthrough of a type that already fits.

**Files:**
- Modify: `packages/zaino-service/src/chain_info.rs`
- Modify: `packages/zaino-service/src/testing.rs`
- Modify: `packages/zaino-core/src/engine/snapshot.rs`
- Modify: `packages/zaino-core/src/passthrough.rs`
- Modify: `packages/zaino-service/usage.md`

**Interfaces:**
- Consumes: `zaino_source::GetBlockchainInfo`, `zaino_primitives::types::BlockchainInfo`.
- Produces: `ChainInfoRead::chain_info() -> Result<BlockchainInfo, ReadError>` (the local `ChainInfo` struct is removed).

- [ ] **Step 1: Find every consumer of `ChainInfo` first**

`grep -rn "ChainInfo" packages/ --include=*.rs` including `tests/`. This changes
a public type in `zaino-service`, so every binder must be updated in the same
commit. Report the list before editing.

- [ ] **Step 2: Write the failing tests**

- `chain_info` returns the scripted `BlockchainInfo` with every field intact —
  **Review Focus 5**: assert `blocks`, `difficulty`, `chain`, `value_pools`,
  `size_on_disk` and `commitments` individually, with distinguishable scripted
  values, so dropping any one field fails the test. A test asserting only
  `blocks` would let the Orchard-pool view blank silently.
- **Review Focus 2:** an unreachable validator errors rather than returning a
  defaulted `BlockchainInfo`. This is the most load-bearing instance of that
  rule in the whole plan: a defaulted success here blanks four explorer views
  for 15 seconds, while an error leaves the previous values in place.

- [ ] **Step 3: Replace the type and implement passthrough**

Delete the two-field `ChainInfo` from `packages/zaino-service/src/chain_info.rs`
and re-point `ChainInfoRead` at `BlockchainInfo`. The engine's existing
`ChainInfoRead` impl in `snapshot.rs` (which derives the aggregate from the
pinned tip) is replaced by passthrough.

**Record in your report why this is whole-passthrough rather than
part-local:** `blocks`, `difficulty` and `commitments` could be read locally,
but mixing a local `blocks` with a passthrough `value_pools` would put two
different chain heights in one response, and this aggregate exists to describe
*one* position. It moves local in one piece when the value-pool cumulative
bridge exists, or not at all.

- [ ] **Step 4: Run the gate and commit**

The full gate — this changes a `zaino-service` public type.

```bash
git add packages/zaino-service packages/zaino-core
git commit -m "refactor(service)!: chain info is the validator's, whole

ChainInfo carried a tip and an estimated height, derived from the pinned tip.
getblockchaininfo needs ten fields, six of which a consumer reads: blocks,
difficulty, chain, valuePools, size_on_disk and commitments. BlockchainInfo
already carries all of them, so the read passes it through.

Whole-passthrough rather than part-local on purpose: blocks, difficulty and
commitments are locally available, but mixing a local blocks with a passthrough
valuePools would report two different chain heights in one response, and this
aggregate exists to describe one position. It localises in one piece when the
value-pool cumulative bridge exists.

BREAKING: ChainInfoRead::chain_info returns BlockchainInfo; the ChainInfo struct
is removed."
```

---

### Task 5: Serve the four methods

**Files:**
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`
- Modify: `packages/zaino-noderpc/src/wire.rs`
- Modify: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/error.rs`

**Interfaces:**
- Consumes: `BlockRead`, `TransactionRead`, `ChainInfoRead` (now `BlockchainInfo`), and `queries` for anything composed.
- Produces: `getblock`, `getblockheader`, `getrawtransaction` at verbosity 1, and a real `getblockchaininfo`.

- [ ] **Step 1: Read the consumer's exact calls first**

The wire shapes come from `nighthawk-apps/zcashex`, not the zcashd prose docs:

- `getblock` — `[hash_or_height_string, verbosity]`, called at **verbosity 2**
  (full transaction detail). A height arrives as a *string*.
- `getblockheader` — `[hash]` only; zcashd defaults `verbose = true`, so the
  verbose shape is the one to serve.
- `getrawtransaction` — `[txid, verbosity]`; verbosity **1** is the decoded
  object. Verbosity 0 already works and must keep working.
- `getblockchaininfo` — no params.

Report the field sets you intend to emit before writing them, so a missing
field is caught in review rather than by a blank explorer page.

- [ ] **Step 2: Serve `getblockchaininfo` first**

It is the smallest of the four and unblocks four explorer views on its own.
Replace the `"estimated_height=N"` stub with a real response struct over
`BlockchainInfo`, using zcashd's field names (`valuePools`, `size_on_disk`,
`estimatedheight`, `bestblockhash`, …) — check each against the explorer's
reads, which are `blocks`, `difficulty`, `chain`, `valuePools`, `size_on_disk`
and `commitments`. Add a serialization test asserting those six keys appear with
zcashd's exact spelling; the renames are the only thing aligning our output with
what the explorer reads.

Delete the now-dead `estimated_height=` formatting and its test.

- [ ] **Step 3: Serve `getblockheader`, then `getblock`**

Both over `BlockRead`. `getblock`'s verbosity-2 shape is the largest wire
rendering in the branch — build it from `Block`'s header, transactions and
chain metadata. Accept the height-as-string form the client sends: parse to a
`Height` if it is all digits, otherwise treat it as a hash. Reject anything that
is neither with a params error.

A height or hash the validator does not know is `Ok(None)` from the read, which
renders as the `NotFound` RPC error the adapter already has.

- [ ] **Step 4: Serve `getrawtransaction` at verbosity 1**

Keep verbosity 0 exactly as it is. Verbosity 1 renders the decoded
`Transaction` from `TransactionRead`. Any other verbosity keeps its existing
explicit params-error refusal — do not silently widen it.

The existing test asserting verbosity 1 is refused must now be **replaced** by
one asserting it is served; say so in your report, since an existing test
changing is otherwise a red flag.

- [ ] **Step 5: Run the gate and commit**

Four commits, one per method, is preferable to one large one — each is
independently reviewable and the diffs are large. Commit forward.

---

## Self-Review

**Spec coverage.** This plan covers the spec's slice 4 (`getblock`,
`getblockheader`, `getrawtransaction` verbosity 1) plus slice 3
(`getblockchaininfo`), pulled forward because both are passthrough and the
latter unblocks four explorer views for little work. It does **not** cover
slice 2 (`getinfo`, `getnetworksolps`, `getmempoolinfo`, `getrawmempool`,
`getmininginfo`, `getpeerinfo`), which remains Tasks 6–9 of the slices-1-2
plan, deferred to after the deploy; nor slice 5 (`getblockhashes`), the only
local read left.

**Placeholders.** Tasks 1 and 5 deliberately ask the implementer to read a
named template or the consumer's call shapes and *report* before writing,
rather than inlining code I have not verified — the `getblock` verbosity-2
rendering and the zebra-rpc deserialization helper are both cases where
guessing produced defects earlier on this branch. Every other step names its
exact types.

**Type consistency.** Task 1 produces `DecodedTransaction`, consumed by Task 3.
Task 4 removes `ChainInfo` and Task 5 Step 2 renders `BlockchainInfo`, so Task 4
must land before Task 5 Step 2. Tasks 2 and 3 both add a module under
`engine/` and both edit `engine.rs` and `passthrough.rs`, so they are
sequential, not parallel.

**Review Focus coverage.** (1) Tasks 2, 3. (2) Tasks 1, 2, 3, 4 — four
instances, because the "transient must not become a defaulted success" rule has
a different consequence at each. (3) Task 2. (4) Task 3. (5) Task 4.

**The milestone.** When Tasks 2 and 3 land, `EngineSnapshot` satisfies
`NodeRpcReads`, so a real engine satisfies `NodeRpcService`. That is the gate on
the deployment + wiring plan, and it is worth noting in the branch's eventual PR
as the thing that changed.
