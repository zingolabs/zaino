# Deferred Scattered Writes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the TransparentHistory initial catch-up stop paying random B-tree inserts. Scattered-key namespaces are deferred to sorted run logs during catch-up, then bulk-appended in key order.

**Architecture:** `EntryCodec` states a `KeyOrder` fact. It flows to the backend through the pipeline `Descriptor` and the namespace list. `Backend` gains provided no-op bulk methods. The LMDB backend implements deferral: a sorted segment log, a manifest committed with the watermark, and a resumable merge with `APPEND`. The store maps incomplete namespaces to `NotServiceable`. The indexer brackets the initial catch-up with `begin_bulk`/`finish_bulk` under a policy knob. A generic backend conformance suite in `zaino-persistence` (feature `testing`) pins the whole `Backend` contract from outside and runs against both backends.

**Tech Stack:** Rust, `lmdb` 0.8 (`WriteFlags::APPEND`), existing `zaino-persistence` / `zaino-persistence-codec` / `zaino-sync` / `zaino-backend-lmdb` / `zaino-store` / `zaino-indexer` / `zaino-runtime` / `zainod`.

**Spec:** `zaino-design/design/deferred-scattered-writes.md` (commit 8041a7d), read it first.

## Global Constraints

- Repo CLAUDE.md rules: no `mod.rs` (foo.rs + foo/); minimum visibility; no `.unwrap()` in production; typed errors with `#[source]`, never stringified; DRY via plain fns; usage.md for any public capability change.
- Rust/Cargo edits only via Edit/Write or `cargo add`. Never sed/perl/python on Rust or Cargo files.
- Commit forward only. Never amend, reset, rebase, stash or force. No `Co-Authored-By` or any attribution trailer. Do not push.
- `cargo check --workspace` cannot pass on this host (aws-lc-sys/binutils). Verify per crate.
- Every task's gate, per touched crate:
  - `cargo test -p <crate> --all-features`;
  - `cargo clippy -p <crate> --no-deps --all-targets --all-features -- -D warnings`;
  - `cargo fmt --check`;
  - `RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps --all-features`. zaino-sync has 15 pre-existing doc errors; do not add new ones.
- With deferral `off` (or a backend that ignores it), behaviour must be byte-for-byte today's.

## Review Focus

1. Crash between run-log fsync and the LMDB txn commit: reopen must truncate the log to the manifest length, and the replayed batch must not duplicate entries.
2. Crash mid `finish_bulk`: resume must neither skip nor duplicate keys (merge-skip ≤ `loaded_through`).
3. A codec wrongly stating `WalkOrdered` must fail loudly (typed error naming the namespace), never silently mis-order.
4. Reads of deferred capabilities during bulk must be `NotServiceable`, never partial or empty-as-success. Compact reads must keep serving.
5. Restart in the middle of bulk mode (indexer resumes from the watermark) must re-enter bulk mode on the existing log, not start a fresh one.

---

### Task 1: `KeyOrder` fact on codecs, carried to the namespace list

**Files:**
- Modify: `packages/zaino-persistence-codec/src/lib.rs` (`KeyOrder` enum, `EntryCodec::KEY_ORDER` with no default)
- Modify: every `impl EntryCodec` (zaino-indexes indexes, zaino-sync toy/testing codecs, codec tests). Let the compiler find them.
- Modify: `packages/zaino-sync/src/descriptor.rs` (+ bridges): `Descriptor` gains `key_order: KeyOrder`, filled from the codec in each bridge constructor (bridges already require `EntryCodec` for persistence; where a bridge does not, thread the bound minimally).
- Modify: `packages/zaino-persistence/src/backend.rs`: `Namespace` list type for backends becomes `NamespaceSpec { namespace, key_order }`. Reserved meta namespaces (`watermark`, `version_meta`) are `WalkOrdered`-irrelevant: give them an explicit `KeyOrder::Meta` or route them separately; pick one and document it.
- Modify: `packages/zaino-backend-lmdb/src/lib.rs` `LmdbConfig::namespaces` → `Vec<NamespaceSpec>`; `packages/zaino-runtime/src/boot.rs` `open_store` builds specs from `pipelines()` descriptors; fix other constructors (zaino-store index_coverage tests, examples).

**Interfaces produced:** `zaino_persistence_codec::KeyOrder { WalkOrdered, Scattered }` (+ `Meta` if chosen); `EntryCodec::KEY_ORDER`; `Descriptor::key_order`; `NamespaceSpec`.

- [ ] Write a failing test in zaino-indexes asserting the expected value per index: headers/txids/hash_to_height/transparent_data/sapling/orchard/ironwood/chain_metadata = `WalkOrdered`; address_history/transparent_spends/txid_location = `Scattered`.
- [ ] Implement, make it pass, and run the gates for codec, indexes, sync, persistence, backend-lmdb, runtime, store.
- [ ] Commit.

### Task 2: Backend bulk contract + generic conformance suite

**Files:**
- Modify: `packages/zaino-persistence/src/backend.rs`:
  - `BulkPolicy` (what `begin_bulk` receives: at least `enabled: bool`; keep it a struct for growth);
  - provided `Backend::begin_bulk(&self, BulkPolicy) -> Result<(), CommitError>` and `Backend::finish_bulk(&self) -> Result<(), CommitError>`, both default `Ok(())`;
  - provided `BackendReader::is_complete(&self, Namespace) -> Result<bool, ReadError>`, default `Ok(true)`.
- Create: `packages/zaino-persistence/src/conformance.rs` (+ `conformance/` submodules as needed), behind feature `testing`. A generic suite over a factory trait:
  ```rust
  pub trait BackendFactory {
      type B: Backend;
      /// A fresh, empty backend with these namespaces.
      fn fresh(&self, namespaces: &[NamespaceSpec]) -> Self::B;
      /// Reopen the same storage after dropping the previous handle (persistence).
      /// Return None if the backend is not persistent (in-memory).
      fn reopen(&self, namespaces: &[NamespaceSpec]) -> Option<Self::B>;
  }
  ```
  and `pub fn run_all<F: BackendFactory>(f: &F)` plus one `pub fn` per property, so a backend crate can call each from its own `#[test]`s.
- Properties to cover. These are existing contract the code already relies on but no test pins generically:
  - get/put/delete round trip;
  - commit atomicity (all ops visible after commit);
  - `scan` and `scan_range` return bytewise key order, and the range is half-open `[lo, hi)`;
  - `first_key`;
  - namespace isolation;
  - reopen persistence (when persistent).
- New bulk properties:
  - with bulk disabled or unsupported, behaviour equals direct;
  - with bulk enabled, after `finish_bulk` every namespace's full scan equals a direct build of the same commits;
  - `is_complete` is `true` for every namespace outside bulk mode;
  - inside bulk mode it is `false` for a `Scattered` namespace only if the backend defers it (the suite must accept both, but must require `true` again after `finish_bulk`);
  - `finish_bulk` is idempotent;
  - begin, commit, reopen, begin, commit, finish equals a direct build (restart in bulk mode; only when `reopen` is `Some`).
- Modify: `packages/zaino-persistence/src/in_memory.rs`: run the suite in its tests (no-op bulk).

**Interfaces produced:** `BulkPolicy`, the three provided methods, `conformance::{BackendFactory, run_all, ...}`.

- [ ] Write the suite, run it against InMemory (passes with defaults), gate, commit.

### Task 3: LMDB: run the suite, `APPEND` for walk-ordered namespaces, typed order error

**Files:** `packages/zaino-backend-lmdb/src/lib.rs` (+ split into `lib.rs` + `lmdb/…` submodules if it grows; no mod.rs), its tests, `Cargo.toml` (`zaino-persistence` `testing` as a dev-dependency feature).

- [ ] Add an LMDB `BackendFactory` (temp dir; `reopen` reuses the path) and call the conformance suite from tests. It must pass before any behaviour change.
- [ ] `WalkOrdered` puts use `WriteFlags::APPEND`. Map `MDB_KEYEXIST` from an append to a new typed `CommitError` variant naming the namespace (out-of-order key for a walk-ordered namespace). Test: a namespace declared `WalkOrdered` receiving a descending key fails with that variant.
- [ ] Rerun the suite and the gates, then commit.

### Task 4: LMDB deferral: run log, manifest, bulk commit, reopen truncation

**Files:** new submodule(s) under `packages/zaino-backend-lmdb/src/` (e.g. `deferred.rs` + `deferred/{log,manifest}.rs`), `lib.rs`.

Spec §3.3, steps 1–4:
- In bulk mode with policy enabled, `Scattered` puts go to a per-namespace segment log `<path>/deferred/<ns>.log`. Each commit sorts its pairs (bytewise key), appends one framed segment, and fsyncs. Then the LMDB txn (meta + walk-ordered ops) also writes manifest key `deferred/<ns>` → `(segment_count, log_len)` in a reserved meta namespace, and commits.
- `Delete` on a deferred namespace in bulk mode → typed error.
- Bulk state is persistent: `begin_bulk` on a store whose manifest has entries re-enters bulk mode, and `open` truncates each log to its manifest length.
- `is_complete(ns)` = no manifest entry for `ns`.
- Segment format: header (magic, entry count, payload length, checksum), then length-framed key/value records. Reading a segment validates the checksum and fails with a typed error.

- [ ] Tests:
  - unit tests for segment encode/decode/checksum/truncation;
  - fault-injected crash after the log fsync and before the txn commit, via a `#[cfg(test)]` hook (on reopen the log is truncated, and replaying the batch yields no duplicates);
  - the bulk conformance properties still pass. `finish_bulk` may be a stub that errors until Task 5, so mark the dependent conformance calls for Task 5 or land Tasks 4 and 5 consecutively.
- [ ] Gate, commit.

### Task 5: LMDB `finish_bulk`: k-way merge + APPEND, resumable

**Files:** `deferred/merge.rs` (or similar), `lib.rs`.

Spec §3.3 `finish_bulk` 1–4:
- k-way merge of the namespace's sorted segments, with bounded buffered readers.
- Equal keys: the later segment wins.
- `APPEND` into the namespace in chunk txns of N entries (const, documented; start at 1,000,000). Each chunk txn records `deferred/<ns>/loaded_through`.
- Resume skips keys ≤ `loaded_through`.
- Final txn: removes the manifest entries; then delete the log.

- [ ] Tests:
  - full bulk conformance suite (equivalence with a direct build, idempotent finish, restart in bulk mode);
  - fault injection mid-merge (crash after chunk k; reopen and finish; equal to direct);
  - equal-key overwrite order.
- [ ] Gate, commit.

### Task 6: Store readiness

**Files:** `packages/zaino-store/src/{address,spend,spend_resolve}.rs` and `index_coverage.rs`, plus the store reader where capability serviceability is decided.

- [ ] Reads backed by `address_history` / `transparent_spends` / `txid_location` check `is_complete` on the pinned reader and return the existing `NotServiceable(Capability::AddressHistory | SpendStatus)` when any backing namespace is incomplete. This follows the spec §3.4 table.
- [ ] `check_index_coverage` (R60) accepts a store whose incomplete namespaces are pending deferral.
- [ ] Tests:
  - with a backend in bulk mode (LMDB, temp dir), address and spend reads are `NotServiceable`; compact reads serve;
  - after `finish_bulk`, they serve and match a direct build.
- [ ] Gate, commit.

### Task 7: Policy + trigger + profiling + docs

**Files:**
- `packages/zaino-indexer/src/source_provisioner.rs`: `begin_bulk` before the initial catch-up `sync_to`, and `finish_bulk` after it, before `reporter.ready()`, when `policy.enabled && (finalised - start) >= threshold` (const 50_000, documented). The engine owns the backend; expose the minimal accessor needed.
- `packages/zaino-runtime` + `packages/zainod` config: `storage.deferred_writes = "auto" | "off"`, default `auto`. Add a fixture env var `ZAINO_TEST_DEFERRED_WRITES` (`auto`/`off`) in `packages/zainod/src/config/fixture.rs`, so the cluster A/B can toggle it.
- `sync-profile` feature: emit one `finish_bulk` timing event per namespace (merge ms, entries, chunk count) in zaino-backend-lmdb.
- usage.md updates: zaino-persistence (bulk contract + conformance suite), zaino-backend-lmdb (deferral, disk budget, crash semantics), zainod (knob), zaino-sync if `Descriptor` docs live there.

- [ ] Tests:
  - the indexer calls `begin_bulk`/`finish_bulk` exactly around catch-up when enabled and above the threshold, and not when `off` or below it (use a recording test backend, i.e. a Backend wrapper that records calls);
  - config parse of the knob and the fixture env.
- [ ] Gates on indexer, runtime, zainod (`--features ztest-fixture` and `--features ztest-fixture,sync-profile` builds), then commit.
