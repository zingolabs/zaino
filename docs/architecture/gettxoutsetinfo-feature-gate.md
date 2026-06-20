# Architecture: gate `gettxoutsetinfo` + txout-set accumulator behind a non-default feature

**Status:** design agreed, **no code written yet**. This document is for refinement and
implementation by another contributor. It is self-contained; you should not need the
originating conversation.

**Companion docs:**
- Decision record: `docs/adr/0002-gettxoutsetinfo-behind-non-default-feature.md` (the "why").
- Cost analysis of the accumulator: `docs/notes/txout-set-accumulator.md`.
- The (speculative) rebuild-and-cutover model this would interact with: `docs/explorations/EXP-0001-rebuild-and-cutover-schema-upgrades.md`.
- Schema/upgrade glossary: `packages/zaino-state/src/chain_index/finalised_state/CONTEXT.md`.

---

## 1. Problem & motivation

`gettxoutsetinfo` is the **sole** consumer of the finalised **txout-set accumulator**
(schema table #9, `tx_out_set_info_accumulator`). The accumulator's from-genesis build
holds the entire spent-set in memory; at mainnet scale (~185M spent outpoints) that is
tens of GiB and OOM-killed a 16 GiB host (#1260). It is also the step that makes every
rebuild-and-cutover (EXP-0001, speculative) expensive. And the RPC is **not implemented by zebra** — the
accumulator exists precisely because the validator cannot serve `gettxoutsetinfo` cheaply.

Our key customer does not need `gettxoutsetinfo`. Goal: put the whole capability —
accumulator table, its build, its write-path maintenance, and the RPC's ability to compute
a result — behind a **non-default Cargo feature `gettxoutsetinfo`**, so the default build
neither stores nor builds the accumulator and pays none of that cost.

## 2. Background facts (verified against the tree)

- **Schema hash is feature-independent.** `DB_SCHEMA_V1_HASH` (`finalised_source/v1.rs:127`)
  is a hardcoded constant = BLAKE2b of the *static* text file
  `finalised_source/db_schema_v1.txt`. Compiling a table out with `#[cfg]` does **not**
  change the hash. Proof: `address_history` (table #10) is `#[cfg]`-gated in 8 places in
  `v1.rs` yet listed unconditionally in `db_schema_v1.txt`. The hash fingerprints the
  layout *contract* (format-if-present), not which optional tables physically exist.
- **The accumulator is rebuildable derived data**, not primary state: a pure function of
  the `spent` + `transparent` + `txid_location` tables, with a watermark
  (`_tx_out_set_accumulator_built_height`, `v1.rs:156`) recording the height it reflects.
- **`spent` (#8) and `txid_location` (#12) are NOT gettxoutsetinfo-specific** and must
  stay: the bulk-sync write path writes both unconditionally (`write_core.rs:114-118`),
  `txid_location` backs write-path previous-output resolution (`db_schema_v1.txt:129-131`),
  and `spent` backs `gettxout`/spent-status. Only the accumulator (#9) is gated.
- **House style for capability gating** is at the storage/capability layer, not the RPC
  layer. The `address_history` gate has **zero** cfg points in `zaino-serve` service,
  the `indexer` trait, or the backends — it gates only the DB/storage layer.
- **`FinalisedStateError::FeatureUnavailable(&str)` already exists** and is the established
  way a backend expresses "I don't have this capability" (e.g. V0/Ephemeral backends return
  it for v1-only methods, `finalised_source.rs:370`).

## 3. Resolved design decisions

| # | Decision | Rationale (short) |
|---|----------|-------------------|
| **D1** | Gate **only** accumulator table #9. Keep `spent`, `txid_location`. | They have non-gettxoutsetinfo consumers (write path, `gettxout`). Rolling back to v1.1.0 would break the write path. |
| **D2** | Mirror `transparent_address_history_experimental`: keep #9 *described* in `db_schema_v1.txt`; `#[cfg]` the physical table. **Schema hash unchanged.** | Zero rebuild-and-cutover (EXP-0001) interaction: no schema fork, no refuse/rebuild. Editing the text file instead would force a from-genesis rebuild on every existing deployment. |
| **D3** | Feature-off: the RPC handler stays registered and returns a typed "unsupported in this build" error (`FeatureUnavailable`). **No validator passthrough.** | Matches house style; stable RPC surface. Passthrough rejected: zcashd-only (breaks under zebra), unbounded validator scan, contradicts rebuild-and-cutover (EXP-0001). |
| **D4** | Gate at the **capability-dispatch seam**. Hard-`#[cfg]` the cost path; keep method *signatures* stable; the dispatch body returns `FeatureUnavailable` when off; everything above the seam is untouched and propagates the error. | Avoids feature-varying trait surfaces (object-safety/bound churn across both backends) while genuinely removing the cost. |
| **D5** | **Non-default / opt-in** (`default = []`); `--features gettxoutsetinfo` to enable. | The default build is the lean, safe, zebra-compatible one; the accumulator cost is paid only by deployments that ask for it. **Breaking change** (default build loses a shipped RPC), documented in CHANGELOGs + ADR-0002. |

## 4. Cross-build / runtime compatibility (consequence of D2)

Because the schema hash is identical with or without the feature, feature-on and feature-off
databases are **mutually openable**; toggling the feature never triggers a *schema* rebuild.

- **Feature-on binary opens a feature-off DB** (accumulator table absent): the watermark is
  absent → the accumulator is built lazily on first sync (the expected "you enabled it, pay
  the build once" path). This is an *index* build, not a rebuild-and-cutover (EXP-0001) schema rebuild.
- **Feature-off binary opens a feature-on DB** (accumulator table present): the field is
  `#[cfg]`'d out, the table handle is never opened; the singleton row is harmless dead weight.

## 5. LSP-verified field-toucher map (authoritative)

`findReferences` on the field `tx_out_set_info_accumulator`
(`finalised_source/v1.rs:289`) returned **20 refs across 4 files** (warm index). The
accessor `tx_out_set_info_accumulator_db()` (`v1.rs:885`) has **1** external caller. Classified:

**A. Field declaration — `#[cfg(feature="gettxoutsetinfo")]` the field:**
- `v1.rs:289`

**B. `DbV1 { … }` construction sites — `#[cfg]` the field-init line at each (mirror the
adjacent `#[cfg] address_history:` line that already sits at every one).**

After the §5.1 and §5.2 pre-refactors this set is **3**, not 7:
- the field declaration (§5A), the `detached_handle` helper (§5.1), and the
  `open_env_and_dbs` helper (§5.2).

Two prior groups are collapsed by the pre-refactors:
- 5 **byte-for-byte identical** detached handle-copies (`v1.rs:525`, `v1.rs:716`,
  `write_core.rs:643`, `write_core.rs:1562`, `compact_block.rs:336`) → one `detached_handle`
  (§5.1).
- 2 near-identical constructors `spawn` (`v1.rs:478`) and `spawn_v1_0_0` (`v1.rs:1042`),
  each itself carrying two `#[cfg]` struct arms → one `open_env_and_dbs` with a single struct
  literal (§5.2).

  ⚠️ Re-run `findReferences` on the field after edits to confirm no new construction site
  was introduced; each site that builds a `DbV1` must gate the field-init or it won't compile.

**C. Accessor — `#[cfg]` the method:**
- `v1.rs:885-887` `tx_out_set_info_accumulator_db()`; its sole caller is the dispatch seam
  `finalised_source.rs:389:47` (see D).

**D. Capability-dispatch seam — branch the BODY, keep the signature (return `FeatureUnavailable`
when off):**
- `finalised_source.rs` `get_tx_out_set_info_accumulator` (method at `:962`; calls the V1
  accessor at `:389`). This is the cut that stops the cfg cascade.
- The V1 trait impl `get_tx_out_set_info_accumulator` at `capability.rs:1071` — keep in the
  trait; impl returns/forwards `FeatureUnavailable` when off.

**E. Cost-path logic (builders/readers/maintenance) — hard-`#[cfg]`:**
- `transparent_address_history.rs`: `build_tx_out_set_accumulator_blocking` (`:1758`) and
  the field-touching reader/update impls at `:749, :1727, :1991, :2201` (LSP refs).
- `write_core.rs:741, :1580` (accumulator maintenance reads/writes on the write path).
- `reader.rs:466` (the field-touching reader, if still present after the seam refactor).
- `migrations.rs:930-948` Stage C (`rebuild_tx_out_set_accumulator()`). NOTE: the speculative EXP-0001 model would (if pursued) plan
  to delete `migrations.rs` entirely; on that branch this site disappears. Handle whichever
  branch you implement against.

**F. Untouched (compile as-is; the `FeatureUnavailable` error propagates):**
- `chain_index.rs:2356` `get_tx_out_set_info` (the NFS fold — short-circuits at the base
  read `:2377` when off), `backends/{fetch,state}.rs`, `indexer.rs` trait,
  `zaino-serve .../service.rs:90,545` handler.
- Metadata types (`FinalisedTxOutSetInfoAccumulator`, `is_unspendable_tx_out`,
  `ZAINO_TXOUTSET_ENTRY_LEN` in `types/db/metadata.rs`) stay compiled (cheap type defs the
  untouched fold references in signatures).

> "Fold" (used above) = reduce over a sequence onto a base, à la `Iterator::fold`. The NFS
> fold starts from the finalised accumulator (base) and walks the non-finalised window
> (~100 blocks), adding NFS-created-still-unspent outputs and subtracting NFS-spent finalised
> outputs to produce the combined best-tip UTXO-set summary.

## 5.1 Pre-refactor (land FIRST, standalone, behavior-preserving): DRY the detached-handle idiom

Five of the construction sites in §5B are the **identical** "detached handle-copy of `self`
for moving into a `spawn`/`spawn_blocking` task" idiom (20 lines, verbatim) —
`v1.rs:525`, `v1.rs:716`, `write_core.rs:643`, `write_core.rs:1562`, `compact_block.rs:336`.
`compact_block.rs:324` already comments that it "mirrors patterns used elsewhere." Extract:

```rust
impl DbV1 {
    /// A detached handle-copy of this DB for moving into a `spawn`/`spawn_blocking` task:
    /// shares the env and atomics (`Arc`), copies the `Database` handles (they're `Copy`),
    /// and resets `db_handler` — the copy is not the background-task lifecycle owner.
    fn detached_handle(&self) -> Self {
        Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            /* …all DB handles… */
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        }
    }
}
```

Each of the 5 sites becomes `let zaino_db = self.detached_handle();`.

- **Visibility:** plain private `fn`. The callers in `v1::write_core` and `v1::compact_block`
  are *child* modules of `v1`; Rust child modules can reach a parent's private items. Do not
  widen (minimum-visibility rule).
- **Why a named fn, not alternatives:**
  - **Not `Clone`** — this is *Clone-except-`db_handler`-reset-to-`None`* (the detached copy
    must not own the join handle). A derive is impossible (`JoinHandle: !Clone`); a hand-written
    `Clone` that silently nulls a field is a surprising `Clone`.
  - **Not `Self { …, ..self }`** — `..self` would *move* the remaining fields, but these sites
    take `&self` and must keep `self` alive (they `Arc::clone` the env and `.clone()` the
    `DashSet`/`config`); the fields aren't all `Copy`, so it won't compile.
  - **Not a macro** — a `fn` expresses it (repo rule: prefer `fn`).
- **Behavior-preserving:** the 5 bodies are textually identical; replacing them with a call to
  a helper holding that same body cannot change behavior. Land it as its own commit (no feature
  flag), verify compiles + tests, then apply §5 gating on top.
- **Scope note:** the 2 genuine constructors (`v1.rs:478`, `:1042`) are handled by §5.2.

Combined with §5.2, the accumulator field's `#[cfg]` surface drops from ~8 construction
sites to **3**: the field declaration (§5A), `detached_handle` (§5.1), and
`open_env_and_dbs` (§5.2).

## 5.2 Pre-refactor (chosen): merge `spawn` / `spawn_v1_0_0` into one opener

`spawn` (production, `v1.rs:346`) and `spawn_v1_0_0` (test-only, `pub(crate)`, `v1.rs:920`)
are **byte-identical from line 1 through the struct construction**: same path setup, same
`max_readers` calc, same `env` open (`set_max_dbs(15)` + `NO_TLS|NO_READAHEAD|NO_SYNC`), the
same 11 `open_or_create_db` calls, and the same two-arm `#[cfg]` struct build. They diverge
**only** in the tail:
- `spawn` → `check_schema_version()` → `reconcile_alpha_txid_location_index()` → `spawn_handler()`
- `spawn_v1_0_0` → writes a `DbMetadata { version 1.0.0, schema_hash [0;32], Empty }` record
  (`v1.rs:1053-1079`), **intentionally skips `check_schema_version`** (`v1.rs:917-919`), →
  `spawn_handler()`

Extract the shared body (Option 3), folding in Option 1's single-literal construction:

```rust
impl DbV1 {
    /// Opens the LMDB env and every V1 named database and builds an *unstarted* `DbV1`
    /// (status `Spawning`, `db_handler` = None, fresh atomics). No metadata validation and
    /// no background task — each caller adds its own tail. Shared by `spawn` and `spawn_v1_0_0`.
    async fn open_env_and_dbs(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        // path setup + max_readers + env open + the 11 open_or_create_db calls …
        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = super::open_or_create_db(
            &env, "address_history_1_0_0",
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED).await?;
        #[cfg(feature = "gettxoutsetinfo")]
        let tx_out_set_info_accumulator = super::open_or_create_db(
            &env, TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME, DatabaseFlags::empty()).await?;

        Ok(Self {
            env: Arc::new(env), headers, txids, /* … */ spent, txid_location,
            #[cfg(feature = "gettxoutsetinfo")]
            tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history,
            metadata,
            validated_tip: Arc::new(AtomicU32::new(0)),
            validated_set: DashSet::new(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
            config: config.clone(),
        })
    }
}
```

Then:

```rust
pub(crate) async fn spawn(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
    let mut zaino_db = Self::open_env_and_dbs(config).await?;
    zaino_db.check_schema_version().await?;
    zaino_db.reconcile_alpha_txid_location_index().await?;
    zaino_db.spawn_handler().await?;
    Ok(zaino_db)
}

pub(crate) async fn spawn_v1_0_0(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
    let zaino_db = Self::open_env_and_dbs(config).await?;
    zaino_db.write_v1_0_0_metadata()?;   // the 1.0.0 metadata block, extracted + named
    zaino_db.spawn_handler().await?;
    Ok(zaino_db)
}
```

**Why this is safe / what to preserve:**
- The helper does **only** open+build — **no policy**. The correctness-sensitive
  "`spawn_v1_0_0` skips `check_schema_version`" stays explicit: the helper never calls it, and
  `spawn` opts in. Do **not** move `check_schema_version` into the helper.
- Extract the `1.0.0`-metadata write into a named `write_v1_0_0_metadata` so the test-only
  logic stays clearly labeled (it uses `block_in_place` + a single rw txn; keep that).
- Folding Option 1 in means the gated `tx_out_set_info_accumulator` open + field-init live in
  exactly **one** place (the helper) instead of across four struct literals.
- Behavior-preserving: the shared prefix is verified byte-identical; the tails are unchanged.
  Verify with `cargo check` (both feature states) + the migration tests, which exercise
  `spawn_v1_0_0`.

**Risks / sequencing (decision: take Option 3 despite these):**
- **Test/prod coupling** — mitigated by keeping all policy in the tails and naming
  `write_v1_0_0_metadata`. Review that `spawn_v1_0_0` still produces 1.0.0 metadata and never
  triggers `check_schema_version`.
- **Migration-refactor churn** — `spawn`/cutover are being reshaped by the in-flight finalised-state migration work. **Coordinate:**
  land this on (or rebased onto) that branch, or as a standalone behavior-preserving commit
  merged before that spawn rewrite, to avoid duplicated/conflicting edits. Do **not**
  develop it in isolation against `migrations.rs`-era code that the migration refactor (cf. speculative EXP-0001) would remove.
- Land §5.1 + §5.2 as their own behavior-preserving commits **first**, then apply §5 gating.

## 6. Cargo wiring (mirror `transparent_address_history_experimental`)

- `packages/zaino-state/Cargo.toml`: add `gettxoutsetinfo = []`; leave `default = []`
  unchanged. Do **not** put it under the `experimental_features` umbrella (it's a shipped
  capability being made opt-in, not experimental).
- `packages/zaino-serve/Cargo.toml`: `gettxoutsetinfo = ["zaino-state/gettxoutsetinfo"]`.
- `packages/zainod/Cargo.toml`:
  `gettxoutsetinfo = ["zaino-state/gettxoutsetinfo", "zaino-serve/gettxoutsetinfo"]`.
- Crate graph: `zaino-state ← zaino-serve ← zainod` (and `zaino-state ← zainod` directly).

## 7. Implementation tooling (Rust-native first)

Use the language's own tooling, not text manipulation, throughout:
- **Navigation / the cfg-site map:** LSP `findReferences` / go-to-def on the
  `tx_out_set_info_accumulator` field and the accessor (as §5 was built) — not `grep`/`rg`.
  Re-run `findReferences` after the §5.1/§5.2 pre-refactors to confirm the site set.
- **Renames:** LSP rename for any symbol rename, so re-exports/impls/macros are followed.
- **Verification:** `cargo check` / `cargo clippy` / `cargo fmt` run for **both** feature
  states (default, and `--features gettxoutsetinfo`); `rustc`/the compiler is the source of
  truth that every `DbV1` construction site and capability-dispatch body still compiles.
- Reserve `grep`/scripted text edits for non-code artifacts (Markdown docs, CHANGELOGs).

## 8. Tests & CI

- Move all accumulator tests under `#[cfg(feature = "gettxoutsetinfo")]`.
- Add one **off-state** test: `get_tx_out_set_info` returns the typed unsupported error, and
  a freshly-spawned DB has **no** accumulator table created.
- **CI matrix must build + test both states**: default (feature off) and
  `--features gettxoutsetinfo`. Without the off-state job the gate bit-rots — the feature-off
  compile (which removes the cost path and must still compile every `DbV1` construction site)
  and the "RPC returns FeatureUnavailable" behavior go unverified.

## 9. Risks / things to verify during implementation

1. **All `DbV1` construction sites** gate the field-init. After the §5.1/§5.2 pre-refactors
   there should be exactly **3** (field decl, `detached_handle`, `open_env_and_dbs`); re-run
   `findReferences` on the field to confirm no straggler construction was reintroduced —
   the in-flight migration work is churning these files.
2. **max_dbs** in `spawn`/`spawn_v1_0_0` (`db_schema_v1.txt` says 12): ensure the LMDB
   `max_dbs` still covers the feature-on table count and the feature-off build opens fewer
   without error. Mirror how `address_history` adjusts the count.
3. **The seam genuinely short-circuits.** Confirm `chain_index.rs:2377` returns/propagates
   the `FeatureUnavailable` before any NFS-fold work, so feature-off has no runtime cost.
4. **`reader.rs:466` vs the seam:** decide whether the reader is removed (folded into the
   seam) or `#[cfg]`'d; keep exactly one field-access path.
5. **Migration-branch skew:** if implementing on the branch that deletes `migrations.rs`,
   §5E's Stage C site is gone; if on the current branch, gate it.
6. **CHANGELOG/ADR already written** (zainod/zaino-serve/zaino-state CHANGELOGs +
   ADR-0002) — keep them in sync if the design is refined.
