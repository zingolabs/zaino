# `getblockhashes` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `getblockhashes(high, low, {noOrphans, logicalTimes})` locally
from the already-synced `HeadersIndex`. No validator implements this method,
and it is the entry point of every block list in the NightHawk explorer.

**Architecture:** the work is a pure consensus fact, a tier read, a local read,
and a wire method.

1. Pure consensus fact: a block's time lies in `(MTP(h-1), MTP(h-1) + 90 min]`.
   `MTP` is the median of the previous 11 block times and is non-decreasing. A
   binary search on MTP therefore yields a contiguous candidate height bracket,
   and filtering the bracket by actual time is exact.
2. A tier read `HeaderRead::header(height) -> Option<HeaderSummary { hash, time }>`
   on the two chain tiers and the routed chain view.
3. A local driving read, `BlockHashRead`, in zaino-core over the chain view.
4. The wire method in zaino-noderpc, with both response shapes.

**Spec:** `zaino-design/design/explorer-grant-completion.md`, section
"`BlockHashRead` and the timestamp trap" (binding):
- "near-monotonic in practice" is not an accepted argument;
- the margin is a named type that states its consensus rule;
- correctness is proven by a property test over out-of-order sequences;
- the read declares `[HeadersIndex]` as its backing set.

## Global Constraints

The tx-view plan's Global Constraints apply unchanged. In short:
- typed errors with `#[source]`, never stringified;
- no `as`, no unwrap/expect in production code, no mod.rs, minimum visibility;
- zaino-core binds resilient source traits only;
- adapters only adapt;
- usage.md for each public capability;
- editor tools only for Rust;
- the full per-crate gate, plus `zaino-service` feature-off;
- commit forward, no trailer, no push, no PR.

Additional constraints for this plan:
- `split_at_seam` and the FS/NFS routing rule are not modified. The chain view
  routes `header(h)` with the same `route()` it uses for `compact_block(h)`.
- Before encoding the time-drift bound, verify it against the Zcash protocol
  specification, including any activation height or network difference. If it
  only applies from some height, the bound type carries that, and the bracket
  falls back to a full scan below it. Cite the spec section in the doc comment.

## Review Focus

1. Out-of-order timestamps. A block whose time is lower than its predecessor's
   is returned exactly when its own time is in range. The property test compares
   against brute force.
2. Range ends. The match is `low <= time < high`. Check zcashd's exact semantics
   in the legacy zaino-fetch/zaino-state implementation or the zcashd RPC docs,
   and pin them with a test.
3. Order of results. zcashd returns ascending by timestamp, then by hash. Mirror
   the legacy implementation exactly and pin it. The explorer reverses the list
   itself.
4. A range beyond the tip, or before genesis, returns an empty list, not an
   error. A range spanning the FS/NFS seam returns both tiers' blocks with no
   gap and no duplicate.
5. `logicalTimes = true` renders `[{blockhash, logicalts}]`, where `logicalts`
   is the block time. `false` or absent renders `[hash]`. A missing options
   object defaults to zcashd's defaults.

---

### Task 1: Median-time-past and the candidate bracket (pure)

**Files:**
- `packages/zaino-consensus/src/` (new module `block_time.rs`), or wherever the
  crate keeps consensus rules; read its lib.rs first.

**Produces:**
- `pub struct MaxBlockTimeDrift`, which documents the rule: nTime ≤ MTP + 90·60,
  with its protocol-spec citation and any activation handling.
- `pub fn median_time_past(times: &[BlockTime]) -> Option<BlockTime>`, the
  median of up to the last 11.
- `pub fn candidate_heights<F>(tip: Height, low: BlockTime, high: BlockTime, time_at: F) -> …`.
  It returns the smallest inclusive height range guaranteed to contain every
  block with `low <= time < high`. `time_at` is an async or sync accessor;
  choose whichever lets zaino-core drive it over tier reads without blocking.
  It binary-searches on MTP and evaluates MTP from 11 `time_at` calls.

Steps:
- [ ] Write the property test first. Generate random chains where each time is
  drawn uniformly from `(MTP(h-1), MTP(h-1)+5400]`, plus fixed adversarial
  sequences: a spike to +5400, then low blocks. For random `[low, high)`, check
  that the blocks in `candidate_heights` filtered by time equal the brute-force
  filter over the whole chain. Use proptest if it is already a dev-dependency
  anywhere; otherwise use a fixed-seed generator with no new dependencies.
- [ ] Add unit tests: genesis region (fewer than 11 predecessors), an empty
  range, `low >= high`.
- [ ] Implement, using checked time arithmetic. Commit.

### Task 2: `HeaderRead` on the chain tiers

**Files:** the tier read trait sits beside `CompactBlockRead` (find where
`ChainTier: ChainSegment + CompactBlockRead` is defined, `zaino-core/src/chain_view.rs:22`,
and where `CompactBlockRead` itself lives). Implementations:
- `zaino-store` `StoreSnapshot`, from `HeadersIndex`;
- `zaino-chain-head-service` `HeadSnapshot`, from the in-memory headers;
- `zaino-core` `ChainViewSnapshot`, routed like `compact_block`;
- the `zaino-core/src/testing.rs` stubs.

**Produces:**
`trait HeaderRead { fn header(&self, h: Height) -> impl Future<Output = Result<Option<HeaderSummary>, BlockReadError>> + Send; }`,
with `HeaderSummary { hash: BlockHash, time: BlockTime }`. `ChainTier` gains it.

- [ ] Write failing tests per implementor. The store reads a persisted header.
  The head tier reads an in-window header. The chain view routes below and above
  the watermark, using distinct values per tier so that a wrong route fails.
- [ ] Implement. Commit.

### Task 3: `BlockHashRead`, a local read in zaino-core

**Files:**
- zaino-service: reads.rs, error.rs, read_sets.rs (add to `NodeRpcReads`),
  testing.rs (mock), usage.md
- zaino-core: `engine/block_hash.rs`, and the capability declaration following
  how other local indexed reads declare `[HeadersIndex]`-style backing (read
  `zaino-indexes/src/capabilities.rs` and the engine manifest)

**Produces:** `trait BlockHashRead { fn block_hashes(&self, low: BlockTime, high: BlockTime) -> impl Future<Output = Result<Vec<BlockHashAt>, BlockHashReadError>> + Send; }`
with `BlockHashAt { height, hash, time }`, ordered per Review Focus 3.
`BlockHashReadError` is a typed enum with `#[source]`. Local only: it reads the
pinned snapshot's chain view, clamped to the pinned tip.

- [ ] Write failing engine tests over mock tiers. Cover:
  - a range inside the FS tier;
  - a range inside the NFS tier;
  - a range across the seam;
  - an out-of-order block;
  - an empty result beyond the tip;
  - ordering.
- [ ] Implement over `candidate_heights` + `HeaderRead`. `assert_node_rpc` and
  the W3 production `NodeRpcRouting` still compile. Commit.

### Task 4: `getblockhashes` on the wire

**Files:** zaino-noderpc `rpc.rs`, `lib.rs`, `wire.rs`, `wire/params.rs`,
`wire/response.rs`, usage.md.

- [ ] Write failing tests. Cover:
  - params `[high, low]`, `[high, low, {}]`, `[high, low, {"noOrphans":true,"logicalTimes":false}]`
    and the logicalTimes `true` form;
  - golden shapes for both renderings;
  - `low > high` mirrors zcashd: check legacy, and either return an empty list
    or invalid-params, justified;
  - a non-integer timestamp is rejected with invalid-params.
- [ ] Implement. Commit.
