# Local Treestate Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `GetTreeState` / `GetLatestTreeState` / `GetSubtreeRoots`
(and `z_gettreestate` / `z_getsubtreesbyindex`) locally from zaino's own index, built
during sync with parallel hashing, byte-exact with zebra.

**Architecture:** Three layers.
1. **Engine.**
   - Fix `LocalBridge` to merge in chain order, and narrow `Monoidal` to non-commutative.
   - Add a declarative `Carry` marker. Its `OrderedMonoid` variant lets the existing
     S×A bridge build a batch in parallel with lift → ordered tree-reduce → per-height
     projection.
   - Build the CrossIndex row: a real `DepsReader` over committed dependency state, and an
     X×A bridge.
2. **Domain.** A commitment-tree `Segment` algebra (generic over the pool's node hash),
   plus the frontier ↔ legacy-encoding conversions.
3. **Index and serving.**
   - `tree_state` (S×A, `Carry = OrderedMonoid`): height → per-pool frontier + size.
   - `subtrees_{sapling,orchard,ironwood}` (X×A over `tree_state` + compact data).
   - Store and chain-head tier reads, a `Local` `TreestatePlacement`, the light-wallet-local
     routing flip, and a cluster validation run.

**Tech Stack:** Rust; `zaino-sync` (engine), `zaino-indexes`, `zaino-store`,
`zaino-chain-head-service`, `zaino-core`, `zaino-runtime`, `zaino-lightserve`,
`zaino-noderpc`; `incrementalmerkletree` 0.8.2, `sapling-crypto` 0.7.0, `orchard` 0.15.3
(already in `Cargo.lock`); `rayon`.

**Spec:** `zaino-design/design/treestate-index.md` (commit 294db34 or later). Also read
`zaino-design/design/index-sync-model.md` §3 and §6.3–6.4.

**Base:** branch `feat/treestate-index`, from `feat/deferred-writes` @ 326516652. It needs
`KeyOrder` / `Descriptor::key_order`, the conformance suite and APPEND. Merge after
deferred-writes lands on #1550.

## Global Constraints

- **Repo rules** (CLAUDE.md, binding):
  - no `mod.rs` (`foo.rs` + `foo/`);
  - minimum visibility;
  - no `.unwrap()` in production; `.expect` only for named invariants;
  - typed errors with `#[source]`, never stringified;
  - DRY via plain fns;
  - `usage.md` for public capability changes;
  - `Persistent*` records cross with `from_business` / `into_business`;
  - wire conversions live in adapter crates.
- **Tool and git rules:**
  - Rust/Cargo edits only via Edit/Write or `cargo add`; never sed/perl/python on
    Rust/Cargo.
  - Commit forward only: never amend, reset, rebase, stash or force.
  - No `Co-Authored-By` or any attribution trailer.
  - Do not push.
  - Commit messages are standalone technical text, why first.
- **Verification:** `cargo check --workspace` cannot pass on this host (aws-lc-sys /
  binutils); verify per crate.
- **Per-task gate, for every touched crate:**
  - `cargo test -p <crate> --all-features`;
  - `cargo clippy -p <crate> --no-deps --all-targets --all-features -- -D warnings`;
  - `cargo fmt --check`;
  - `RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps --all-features`.

  zaino-sync has exactly 15 pre-existing doc errors; add none.
- **`Monoidal` contract:** associative, with identity, NOT commutative; combine order =
  chain order. Never rely on commutativity anywhere.
- **Byte order:** tree roots are held in internal byte order. `finalsaplingroot` on the
  wire is display (reversed) order; `finalorchardroot` and subtree roots are not reversed.
  This was verified against zebra by `bench/treestate` on 2026-10-07; re-verify in tests.
- **Empty tree on the wire:** `""` below a pool's activation height; the serialised
  empty tree from activation on. `GetTreeState` returns exactly the requested height.

## Review Focus

1. **Batch boundary inside a subtree, and a pool's activation height inside a batch.**
   Per-height frontiers and subtree roots must equal a sequential `Frontier::append` at
   every height. (Task 4 property test with random batch splits and activation offsets.)
2. **Restart mid-batch.** Resume must reload the carry from the stored frontier at the
   watermark and continue byte-identical to an uninterrupted build. (Task 5 resume test.)
3. **Reorg in the recent window.** The window refolds from the finalised frontier; a
   tree state served after the reorg reflects the new branch. (Task 8 NFS reorg test.)
4. **Heights below activation and above the tip.** Below activation: `None` for that pool
   (`""` on the wire). Above the tip: `NotServiceable` / not-found, never an empty tree.
   (Task 8 read tests and Task 9 wire golden.)
5. **Spam-era density.** Batches with hundreds of thousands of leaves must not blow memory
   or fall back to serial hashing. (Task 4 large-batch test: 500k leaves within a
   documented memory bound; Task 10 cluster profile.)

---

### Task 1: `LocalBridge` merges in chain order; `Monoidal` narrowed

**Why:** `LocalBridge::extract_one` pushes deltas in rayon completion order and `merge`
folds them in that order. That is correct only for commutative combines, and it is a
latent bug for `(BlockLocal, Fold)`. Spec §3.1–3.2.

**Files:**
- Modify: `packages/zaino-sync/src/bridge.rs` (`LocalBridge`: store `(offset, delta)`;
  sort by offset in `merge`).
- Modify: `packages/zaino-sync/src/pipeline.rs` / `engine.rs`: `extract_one` must receive
  the block's offset. Find where `run_extractions_parallel` calls
  `pipeline.extract_one(ctx)`, and pass `job.global_offset`.
- Modify: `packages/zaino-sync/src/descriptor.rs` and `traits.rs` (docs on `Monoidal`,
  `CompositionType::Monoidal`, `MergeMonoidal::combine`).
- Create: `packages/zaino-sync/src/testing/toy_indexes/concat_index.rs`
  (BlockLocal×Monoidal) and `concat_fold_index.rs` (BlockLocal×Fold). Both are
  non-commutative: the delta is a `String` with the block height, and combine/fold is
  concatenation.
- Modify: `zaino-design/design/index-sync-model.md` §3.2: M is an associative monoid with
  identity, *not* commutative; merge order = chain order. Separate repo; separate commit.

**Interfaces:**
- Produces: `IndexPipeline::extract_one(&self, offset: BlockOffset, ctx: &Ctx)` (the new
  parameter). Every bridge accepts it. Cumulative bridges may ignore it, because their
  extraction is already sequential.

- [ ] **Step 1: Write the failing tests** in `concat_index.rs` / `concat_fold_index.rs`.
  Sync 50 blocks in batches of 7 with the engine's real parallel extraction, and repeat 20
  times so rayon order varies. Assert the merged value equals the heights concatenated in
  chain order:

```rust
#[test]
fn monoidal_merge_follows_chain_order_under_parallel_extraction() {
    for _ in 0..20 {
        let merged = run_toy_sync::<ConcatIndex>(50, 7);
        assert_eq!(merged, (0..50).map(|h| h.to_string()).collect::<Vec<_>>().join(","));
    }
}
#[test]
fn fold_merge_follows_chain_order_under_parallel_extraction() { /* same, ConcatFoldIndex */ }
```

  `run_toy_sync` reuses the helpers in `testing/toy_indexes.rs`.
- [ ] **Step 2:** Run `cargo test -p zaino-sync --all-features concat`. Expected: FAIL
  (order mismatch) on at least one iteration. If it passes on this machine, force
  disorder: run the batch through a test-only pool that reverses job order (a
  `#[cfg(test)]` hook in `run_extractions_parallel`), so the test is deterministic.
- [ ] **Step 3:** Implement offset-tagged deltas: `deltas: Mutex<Vec<(BlockOffset,
  I::Delta)>>`, and in `merge` use `sort_unstable_by_key(|(o, _)| *o)` before
  `S::merge_deltas`. Update the docs. Example `Monoidal` doc: "Overlapping keys combined by
  an associative operation with identity. NOT assumed commutative: the engine combines in
  chain order. A commutative fast path must be a separately named composition."
- [ ] **Step 4:** Rerun; PASS. Run the full zaino-sync gate.
- [ ] **Step 5:** Commit (zaino) and commit `index-sync-model.md` (zaino-design).

### Task 2: Declarative `Carry` marker

**Why:** Spec §3.3 gap 1. The carry algebra becomes a type-level declaration the engine
reads.

**Files:**
- Modify: `packages/zaino-sync/src/descriptor.rs`: sealed markers `Sequential` and
  `OrderedMonoid`, trait `CarryAlgebra` with a `const VALUE: CarryType`, enum `CarryType`
  (strum Display), and `Descriptor::carry: Option<CarryType>` (`None` for non-cumulative
  scopes).
- Modify: `packages/zaino-sync/src/traits.rs`: `ExtractCumulative` gains
  `type Carry: CarryAlgebra;` (no default). The bridge constructors that build the
  descriptor fill `carry`.
- Modify: every `ExtractCumulative` impl: `chain_metadata` (`Sequential`) and the toys
  `cumulative_sum`, `cumulative_series` (`Sequential`). Let the compiler find them.

**Interfaces:**
- Produces: `zaino_sync::descriptor::{Sequential, OrderedMonoid, CarryAlgebra,
  CarryType}`; `ExtractCumulative::Carry`; `Descriptor::carry`.

- [ ] **Step 1:** Failing test in `descriptor.rs` tests: `ChainMetadataIndex`'s descriptor
  reports `carry == Some(CarryType::Sequential)`; `HeadersIndex` reports `None`.
- [ ] **Step 2:** Run; FAIL (field missing).
- [ ] **Step 3:** Implement the markers and the field; update all impls.
- [ ] **Step 4:** Gate: zaino-sync, zaino-indexes, zaino-store (it builds descriptors),
  zaino-runtime build.
- [ ] **Step 5:** Commit.

### Task 3: CrossIndex row: real `DepsReader` + X×A bridge

**Why:** Spec §3.3 gap 2. Subtree roots are `X({tree_state, compact pool index}) × A` with
an `R≤` read pattern.

**Files:**
- Modify: `packages/zaino-sync/src/traits.rs`: replace the empty `pub struct DepsReader;`
  with a handle over the committed backend state of the declared dependencies:

```rust
pub struct DepsReader<'a> {
    reader: &'a dyn BackendReader,
    allowed: &'a [IndexId],
}
impl DepsReader<'_> {
    /// Typed point read of a dependency's committed entry; refuses undeclared dependencies.
    pub fn get<D: IndexDef + EntryCodec>(&self, key: &D::Key) -> Result<Option<D::Value>, DepsReadError>;
}
```

  `DepsReadError`: `Undeclared { dependency }`, `Read(#[source] ReadError)`,
  `Decode(#[source] DecodeError)`. `ExtractCross::extract(ctx, &DepsReader)` keeps its
  signature with the lifetime added.
- Modify: `packages/zaino-sync/src/bridge.rs`: `BridgeDispatch` for `(CrossIndex, Append)`
  → `LocalBridge`-like parallel extraction that receives a `DepsReader` built from a fresh
  backend reader per batch.
- Modify: `packages/zaino-sync/src/scheduler.rs` / `engine.rs`. Verify, and if missing
  implement, that a CrossIndex's extraction of batch β is released only after every
  dependency has **committed** β (`FiringRule::Pipelined`; `dag.rs` already derives
  firing). Today `scheduler.rs:210` treats CrossIndex like SelfCumulative ("one at a
  time"). An X×A index is block-parallel once its gate opens. Correct that comment and
  behaviour.
- Create: `packages/zaino-sync/src/testing/toy_indexes/cross_double_index.rs`: an
  X({value toy}) × A index whose entry at h = 2 × the `value` toy's entry at h.

**Interfaces:**
- Consumes: Task 1's `extract_one(offset, ctx)`.
- Produces: `DepsReader::get::<D>(&key)`; X×A dispatch; release of a pipelined dependency
  gate after the dependency's batch commit.

- [ ] **Step 1:** Failing tests:
  - (a) the toy cross index produces 2× the dependency's values for 40 blocks in batches
    of 6;
  - (b) `DepsReader::get` on an undeclared index returns `Undeclared`;
  - (c) ordering: a cross extraction for batch β never runs before the dependency's commit
    of β. Assert via a recording backend that the dependency's commit for β precedes any
    `DepsReader` read for β heights.
- [ ] **Step 2:** Run; FAIL (unimplemented dispatch).
- [ ] **Step 3:** Implement. Keep `DepsReader` construction per batch (one backend reader
  pinned per batch), not per block.
- [ ] **Step 4:** Gate zaino-sync (+ zaino-persistence if the error types change).
- [ ] **Step 5:** Commit.

### Task 4: Ordered-monoid carry in the S×A bridge

**Why:** Spec §3.2 item 4, refined. A per-block lift, then an ordered tree-reduce, then
per-height projection by lookup, so every node is hashed once, in parallel, with no serial
seam pass.

**Files:**
- Modify: `packages/zaino-sync/src/traits.rs`: new trait, required when
  `Carry = OrderedMonoid`:

```rust
/// Carry threading as an ordered monoid with a measure (index-sync-model §6.3).
pub trait OrderedMonoidCarry: CumulativeAppend {
    /// Additive, commutative position measure (e.g. per-pool leaf counts).
    type Measure: Copy + Send + Sync;
    fn measure_of(ctx: &Self::BlockContext) -> Self::Measure;
    fn measure_add(a: Self::Measure, b: Self::Measure) -> Self::Measure;
    /// Measure at the end of the state the carry represents.
    fn carry_measure(carry: &Self::PriorState) -> Self::Measure;
    /// Summary of a contiguous run of blocks; ordered monoid under `combine`.
    type Segment: Send;
    fn lift(ctx: &Self::BlockContext, start: Self::Measure) -> Result<Self::Segment, Self::Error>;
    fn carry_segment(carry: &Self::PriorState) -> Self::Segment;
    fn identity() -> Self::Segment;
    /// Associative, NOT commutative: `a` precedes `b`.
    fn combine(a: Self::Segment, b: Self::Segment) -> Self::Segment;
    /// The per-height value whose end measure is `at`, read from a segment that covers
    /// everything up to `at` (including the carry). Pure lookup; no hashing required.
    fn project(full: &Self::Segment, at: Self::Measure) -> Self::Value;
}
```

- Modify: `packages/zaino-sync/src/bridge.rs` (`CumulativeAppendBridge`). Under
  `Carry = OrderedMonoid`:
  1. sequential measure prefix from `carry_measure(carry)`;
  2. `lift` each block in parallel (rayon `par_iter` over the batch);
  3. ordered tree-reduce with rayon `reduce(identity, combine)`, which preserves order for
     associative operations;
  4. `full = combine(carry_segment(carry), batch)`;
  5. per-height values via `project(&full, end_measure_h)`, in parallel;
  6. new carry = the last height's value (PriorState = Value).

  Selection is type-level in `BridgeDispatch` for `(SelfCumulative, Append)` on the
  `Carry` marker. `Sequential` keeps today's path unchanged.
- Create: `packages/zaino-sync/src/testing/toy_indexes/toy_merkle_index.rs`: a toy S×A
  index with `Carry = OrderedMonoid` over a tiny binary Merkle tree of u64 leaves with a
  non-commutative toy hash (e.g. `blake2b(left || right)` truncated). This makes the bridge
  testable without the zcash crates.

**Interfaces:**
- Consumes: Task 2's `Carry` marker.
- Produces: the `OrderedMonoidCarry` trait; the S×A bridge's scan path.

- [ ] **Step 1:** Failing property tests (proptest, already a dev-dep if present; else
  `cargo add --dev proptest`):
  - (a) for random leaf counts per block (0–300), random batch sizes (1–50) and a random
    start carry, every per-height value from the OrderedMonoid path equals the same index
    folded sequentially (a reference `fold` in the test);
  - (b) the same under a forced reversed job order (Task 1's test hook);
  - (c) resume: stop after k batches, rebuild the bridge (carry reloaded from the stored
    value), continue; the output equals an uninterrupted run;
  - (d) a large batch of 500,000 leaves completes and stays within the memory bound
    documented in the toy (assert segment node count ≤ 2 × leaves).
- [ ] **Step 2:** Run; FAIL (trait or path missing).
- [ ] **Step 3:** Implement the bridge path.
- [ ] **Step 4:** Gate zaino-sync.
- [ ] **Step 5:** Commit.

### Task 5: Commitment-tree segment algebra + frontier codec

**Why:** The domain implementation of `OrderedMonoidCarry` for real pools, and the
legacy wire encoding.

**Files:**
- Create: `packages/zaino-indexes/src/indexes/tree_state.rs` + `tree_state/`:
  - `segment.rs`: `TreeSegment<H: Hashable + Clone>`. It retains every complete node in
    `[start, end)` per level (dense per-level vecs with base offsets), plus the carried
    left-edge nodes when built from a frontier.
    - `TreeSegment::lift(leaves: &[H], start: u64)`: hashes level by level, parallel
      across a level via rayon chunks.
    - `combine(a, b)`: requires `a.end == b.start`; computes the ≤1-per-level seam nodes.
    - `frontier_at(&self, size: u64) -> Option<NonEmptyFrontier<H>>`: lookup only.
  - `codec.rs`: `PersistentTreeStateValue` (per pool: size u64 BE + frontier ommers +
    leaf, the existing `incrementalmerkletree` frontier serialisation where available)
    with `from_business` / `into_business`. Plus
    `legacy_tree_bytes(frontier) -> Vec<u8>`, zcashd's `CommitmentTree` encoding as
    `z_gettreestate` `finalState` uses it.
  - `pools.rs`: the Sapling (`sapling_crypto::Node`), Orchard and Ironwood
    (`orchard::tree::MerkleHashOrchard`) leaf conversions from `cmu` / `cmx` bytes.
- Test fixtures: `packages/zaino-indexes/tests/fixtures/treestate/` with zebra
  `z_gettreestate` responses for heights {419200, 419201, 903000, 1687104, 1687105,
  2000000, 3400000}, and `z_getsubtreesbyindex` sapling/orchard 0..3. Capture with an
  in-cluster job against `zebra.zebra-lazy-v642` (see `bench/treestate` for the RPC
  client). Commit the JSON.

**Interfaces:**
- Produces: `TreeSegment<H>` with `lift` / `combine` / `frontier_at` / `node_at(level,
  index)`; `legacy_tree_bytes`; the per-pool leaf conversions.

- [ ] **Step 1:** Failing tests:
  - (a) property: for random leaves and random split points, `combine` of `lift`ed parts,
    then `frontier_at(n)`, equals sequential `Frontier::append` for every n. Use the
    Sapling and Orchard node types, plus a random non-zero `start`;
  - (b) associativity: `(a⊕b)⊕c == a⊕(b⊕c)` node-for-node; non-commutativity is
    witnessed;
  - (c) golden: `legacy_tree_bytes` for the frontier rebuilt from fixture leaves equals the
    zebra fixture `finalState` (small heights only, or take fixture frontiers directly);
  - (d) `PersistentTreeStateValue` round trip and fingerprint samples.
- [ ] **Step 2:** Run; FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Gate zaino-indexes.
- [ ] **Step 5:** Commit.

### Task 6: `tree_state` index (S×A, `Carry = OrderedMonoid`)

**Files:**
- Modify: `packages/zaino-indexes/src/indexes/tree_state.rs`: `TreeStateIndex`.
  - `IndexDef`: `Scope = SelfCumulative`, `Composition = Append`, `NAME = "tree_state"`,
    `BlockContext = TreeStateCtx { height, sapling_cmus, orchard_cmxs, ironwood_cmxs }`.
  - `ExtractCumulative`: `Carry = OrderedMonoid`, `PriorState = Value = TreeStateValue`
    (per-pool frontier + size).
  - `CumulativeAppend`; `OrderedMonoidCarry` with `Measure` = per-pool sizes and
    `Segment` = the three per-pool `TreeSegment`s.
  - `EntryCodec`: `Key = BlockHeight` (`HeightKey`), `KEY_ORDER = WalkOrdered`.
- Modify: `packages/zaino-indexes/src/sets/current_zaino.rs`: add the `ProvideContext`
  projection.
- Create: `packages/zaino-indexes/src/sets/light_wallet_local.rs`:
  `LightWalletLocal` = `TransparentHistory`'s indexes + `TreeStateIndex` (Task 7 adds the
  subtree indexes).

**Interfaces:**
- Consumes: Tasks 2, 4 and 5.
- Produces: `TreeStateIndex`, `TreeStateValue { sapling, orchard, ironwood:
  PoolFrontier }`, the `LightWalletLocal` index set.

- [ ] **Step 1:** Failing tests:
  - (a) a toy chain of 30 blocks with mixed pool activity (from `CurrentZainoContext`
    fixtures), synced through the real engine on the in-memory backend in batches of 7:
    every stored frontier equals sequential appends;
  - (b) the same on LMDB (temp dir);
  - (c) resume from a mid-chain watermark equals an uninterrupted build.
- [ ] **Step 2:** Run; FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Gate zaino-indexes, zaino-sync.
- [ ] **Step 5:** Commit.

### Task 7: Subtree-root indexes (CrossIndex)

**Files:**
- Create: `packages/zaino-indexes/src/indexes/subtrees.rs` + `subtrees/`: a generic
  `SubtreesIndex<P: Pool>` instantiated three times (`subtrees_sapling`,
  `subtrees_orchard`, `subtrees_ironwood`).
  - `Scope = CrossIndex`, `DEPENDENCIES = [tree_state, <pool compact index>]`,
    `Composition = Append`.
  - Key = subtree index (u32 BE, `WalkOrdered`); Value = (root, completing height).
  - Extraction:
    - from `TreeStateCtx` sizes, decide whether block h completes ≥1 subtree;
    - if not, emit nothing;
    - if so, read `tree_state` at h−1 via `DepsReader`, append this block's leaves,
      and capture the completed level-16 nodes.
- Modify: `light_wallet_local.rs`: add the three subtree indexes.

**Interfaces:**
- Consumes: Task 3 (`DepsReader`, X×A), Task 5 (`TreeSegment`), Task 6 (`TreeStateIndex`
  key and value).
- Produces: the `SubtreesIndex<P>` entries.

- [ ] **Step 1:** Failing tests:
  - (a) a toy chain where subtrees complete mid-block, at a block boundary, and two in one
    block (use a test-only subtree level of 2 for small fixtures). The emitted roots equal
    the level-k nodes of a sequential tree;
  - (b) golden: the first three Sapling / Orchard roots equal the Task 5 fixtures. This
    needs real mainnet leaves for those ranges, so gate it behind an `#[ignore]` cluster
    test if the leaves can't be committed.
- [ ] **Step 2:** Run; FAIL.
- [ ] **Step 3:** Implement. The subtree level is a const of 16, with a test override.
- [ ] **Step 4:** Gate.
- [ ] **Step 5:** Commit.

### Task 8: Local reads across the seam

**Files:**
- Create: `packages/zaino-store/src/tree_state.rs`: the finalised-tier
  `TreestateRead`-shaped reads over `tree_state` and `subtrees_*`.
  - Readiness: `NotServiceable(Capability::Treestate | SubtreeRoots)` above coverage.
  - The `Treestate` domain value: `final_state` = `legacy_tree_bytes`; `final_root` =
    `Some(root)`; pools below activation = `None`.
  - Block hash and time come from `headers`.
- Create: `packages/zaino-chain-head-service/src/tree_state.rs`: the window tier.
  - Seed from the finalised frontier at the watermark (a point read via the store reader
    the window already holds).
  - Fold each window block serially. Window batches are small; reuse `TreeSegment::lift`
    for one block, then `combine`.
  - On a reorg, refold from the watermark.
  - Expose the same read shape.
  - This also supplies the window's tree sizes. Remove the `tree_size = 0` gap if it is
    in scope (check `project_nfs_treesize_seam_gap`); otherwise leave a typed TODO in the
    ledger, not the code.
- Modify: `packages/zaino-core/src/engine/treestate.rs`: `impl TreestatePlacement for
  Local`. It routes by height across the seam, like the address reads do, and
  `subtree_roots` reads the finalised tier, then the window's newly completed ones.
- Modify: `packages/zaino-core/src/routing/light_wallet.rs`: `LightWalletLocalRouting`
  `type Treestate = Local`. Update the routing table test.
- Modify: `packages/zaino-runtime/src/deployment/light_wallet/local.rs`: index set
  `LightWalletLocal`; drop the treestate source port from that deployment's source bundle
  if the compiler allows.

**Interfaces:**
- Consumes: Tasks 6 and 7.
- Produces: the `Local` treestate placement; the light-wallet-local deployment serving
  treestate locally.

- [ ] **Step 1:** Failing tests:
  - (a) store reads on an LMDB-built toy chain: exact height, below activation → `None`
    pool, above coverage → `NotServiceable`;
  - (b) a window reorg test (pattern from the existing chain-head-service reorg tests):
    after a reorg, a treestate in the window reflects the new branch;
  - (c) a seam test: a height just above the watermark is answered by the window tier, one
    at the watermark by the store, and they agree;
  - (d) routing table test: Treestate = Local for light-wallet-local only.
- [ ] **Step 2:** Run; FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Gate zaino-store, zaino-chain-head-service, zaino-core, zaino-runtime;
  build zainod (`--features ztest-fixture`).
- [ ] **Step 5:** Commit.

### Task 9: Wire byte-exactness

**Files:**
- Modify: `packages/zaino-lightserve` (`GetTreeState`, `GetLatestTreeState`,
  `GetSubtreeRoots` rendering) and `packages/zaino-noderpc` (`z_gettreestate`,
  `z_getsubtreesbyindex`). Verify, and fix if needed, the hex encoding, the byte order
  (Global Constraints), the empty-tree rule, the Ironwood field, and the exact-height
  answer.
- Tests: golden vs the Task 5 zebra fixtures, through each adapter with a mock snapshot
  returning the locally built `Treestate`.

- [ ] **Step 1:** Failing golden tests per adapter (render the domain value built from
  fixture frontiers; compare to zebra JSON / the proto fields).
- [ ] **Step 2:** Run; FAIL where the rendering differs.
- [ ] **Step 3:** Fix the rendering in the adapters only.
- [ ] **Step 4:** Gate zaino-lightserve, zaino-noderpc.
- [ ] **Step 5:** Commit; update `usage.md` for zaino-indexes (`tree_state`, subtrees),
  zaino-store, zaino-core (Local treestate placement), zaino-runtime (the
  light-wallet-local index set).

### Task 10: Cluster validation (not code; the controller runs it)

- [ ] Build the image (sync-profile + ztest-fixture). Run a full sync of
  `light-wallet-local` from genesis on tekau with deferred writes `auto`. Record wall
  time, `tree_state` merge/persist share, and disk size of `tree_state` + subtrees.
- [ ] Byte-compare `GetTreeState` at 50 random heights (incl. activation heights and spam
  era), plus every `GetSubtreeRoots`, against zebra (in-cluster pod, like `dw-compare.py`).
- [ ] Run `wallet-sync` against the deployment. Then ask the user before running
  `wallet-payment` (real funds).
- [ ] Ledger the results; update the spec §5 with measured costs.
