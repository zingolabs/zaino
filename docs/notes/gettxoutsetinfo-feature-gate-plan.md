# Working state: gate gettxoutsetinfo + accumulator behind a non-default feature

Session was a `/grill-with-docs` grilling on this plan; reboot mid-grill. This file +
the memory pointer `project_gettxoutsetinfo_gate.md` capture the state so the grill resumes.

## ⚠️ Tooling gotcha (this session)
`rg` output was **display-mangled**: matched substrings rendered as `n` / `ln`
(e.g. `get_tx_out_set_info` → `n`, `gettxoutsetinfo` → `ln`). DO NOT trust rg symbol
output. Use `git grep -n`, `Read`, or the LSP. All file:line anchors below were
confirmed via `git grep -n` / `Read`, not rg.

## Task
Key customer does NOT need `gettxoutsetinfo`. That RPC is the sole consumer of the
finalised **txout-set accumulator**, whose from-genesis build is the OOM/expense analyzed
in `docs/notes/txout-set-accumulator.md` and the thing that makes EXP-0001 rebuilds costly.
Put the whole gettxoutsetinfo service + accumulator behind a **non-default Cargo feature**
(proposed name: `gettxoutsetinfo`). Feature-off = "current schema minus the accumulator
table (#9)".

## RESOLVED decisions
- **Q1 — target schema = current 12-table schema MINUS table #9 only.**
  KEEP `spent` (#8) and `txid_location` (#12): they have non-gettxoutsetinfo consumers.
  Write path writes BOTH unconditionally (`write_core.rs:114-118`); `txid_location` is
  needed for write-path prev-output resolution (`db_schema_v1.txt:129-131`); `spent` for
  `gettxout`/spent-status. Do NOT roll back to v1.1.0 (would rip out write-path code).
  Gate ONLY #9 + its build + the RPC.

- **Q2 — c1: mirror the `transparent_address_history_experimental` pattern.**
  Keep table #9 DESCRIBED in `db_schema_v1.txt`; `#[cfg(feature="gettxoutsetinfo")]` the
  field/creation/build/maintenance/capability. **Schema hash UNCHANGED.**
  Proof it's feature-independent: `DB_SCHEMA_V1_HASH` is a hardcoded const = BLAKE2b of the
  static text file (`v1.rs:127`); `address_history` (#10) is `#[cfg]`-gated in 8 places in
  `v1.rs` yet listed unconditionally in `db_schema_v1.txt`. ⇒ **ZERO EXP-0001 interaction**:
  no schema fork, no hash mismatch, no refuse/rebuild. Cross-build compat is handled by the
  accumulator's existing watermark `_tx_out_set_accumulator_built_height` (`v1.rs:156`):
  feature-on opening an accumulator-absent DB builds it lazily; feature-off opening an
  accumulator-present DB ignores the dead singleton.
  REJECTED c2 (editing `db_schema_v1.txt` → one-time hash change → forces an EXP-0001
  rebuild on EVERY existing deployment, for bookkeeping only).

- **Q3 — (b): feature-off keeps the RPC registered, returns a typed "unsupported in this
  build" error** via the capability model. House style gates at storage/capability layer,
  NOT the RPC layer (`address_history` has 0 cfg points in `service.rs`/`indexer.rs`/
  backends). REJECTED (c) validator passthrough: `JsonRpSeeConnector::get_tx_out_set_info`
  exists (`zaino-fetch/src/jsonrpsee/connector.rs:892`) BUT is **zcashd-only** — zebra does
  NOT implement gettxoutsetinfo (that absence is WHY zaino built the accumulator), so
  passthrough silently breaks under the strategic zebra direction; also dumps an unbounded
  scan on the validator; also contradicts EXP-0001's explicit rejection of passthrough for
  capabilities.  [User had not yet typed "accept" for Q3 at reboot — confirm on resume.]

## File:line anchors (confirmed via git grep / Read)
- Schema doc (hash source, 12 tables): `…/finalised_source/db_schema_v1.txt`
  - #8 `spent`; #9 `tx_out_set_info_accumulator` (singleton, gettxoutsetinfo-only);
    #10 `address_history` (cfg-gated precedent); #12 `txid_location` (desc says write-path
    prev-output resolution AND accumulator).
- `…/finalised_source/v1.rs`: `DB_SCHEMA_V1_HASH`=127; accumulator table-name const 140-141;
  watermark key 156; `ACCUMULATOR_INCREMENTAL_MAX_GAP=1000` @169; old pinned
  `ACCUMULATOR_BUILD_SHARDS=1` @179. address_history cfg points: 48,298,433,526,543,666,717,998.
- Accumulator builder: `…/finalised_source/v1/transparent_address_history.rs:1758`
  `build_tx_out_set_accumulator_blocking(db_tip, shards)`; shard math 1766-1773; spent_set
  HashSet 1782-1790; block scan + membership 1843,1857-1859.
- Migration (v1.1→v1.2) stages in `…/finalised_state/migrations.rs`: Stage A txid_location
  ~625; Stage B spent ~762; **Stage C accumulator build 930-948** (`rebuild_tx_out_set_accumulator()`).
  NB: EXP-0001 plans to DELETE migrations.rs, but it exists on this branch → gating must
  cover Stage C while it's here.
- Write-path build trigger: `…/finalised_source/v1/write_core.rs:102-118` (steady-state poll
  does NOT rebuild when no new blocks; bulk path writes spent/txid_location).
- RPC stack (method = `get_tx_out_set_info`): `zaino-serve/.../service.rs:90` (trait), `:545-548`
  (impl); `indexer.rs` trait; `zaino-state/backends/fetch.rs:465-466` + `backends/state.rs`
  (both delegate to `self.indexer.get_tx_out_set_info()`); `zaino-state/chain_index.rs`
  `ChainIndex::get_tx_out_set_info` folds finalised accumulator + NFS on top.
- Validator client method (passthrough, REJECTED): `zaino-fetch/src/jsonrpsee/connector.rs:892`.
- Reader/capability: `…/finalised_state/reader.rs:462`; `…/finalised_state/capability.rs:702,1056`;
  guard `require_v1("v1 txout-set accumulator builder")`.

## GRILL COMPLETE — all decisions resolved. Remaining work is implementation.

- **Q3 = (b)** accepted: RPC stays registered, returns typed "unsupported in this build"
  error (`FinalisedStateError::FeatureUnavailable`). No validator passthrough.
- **Q4 = Strategy A** accepted: gate at the **capability-dispatch seam**. Hard-`#[cfg]`
  ONLY the cost path; keep method signatures stable; the dispatch body returns
  `FeatureUnavailable` when off; everything above the seam (NFS fold, backends, indexer
  trait, JSON-RPC handler) is untouched and propagates the error.
- **Q5 = (i) non-default / opt-in** accepted, with the breaking change documented.

### Documentation written this session
- `docs/adr/0002-gettxoutsetinfo-behind-non-default-feature.md` (the decision + rejected options).
- Breaking-change CHANGELOG notes in `packages/{zainod,zaino-serve,zaino-state}/CHANGELOG.md`.
- `CONTEXT.md` Schema-hash entry refined (feature-gated tables described-but-optional).

### Implementation checklist (NOT yet done — no code written)
Hard-`#[cfg(feature = "gettxoutsetinfo")]` (cost path):
- `v1.rs`: `tx_out_set_info_accumulator` field; its creation in `spawn*`; max_dbs count.
- `transparent_address_history.rs`: `build_tx_out_set_accumulator_blocking` and the V1
  reader/update impls at 341, 742.
- `reader.rs:466` (field-touching reader).
- `write_core.rs`: accumulator maintenance calls (the rebuild/update on the write path).
- `migrations.rs:930-948` Stage C (disappears on the EXP-0001 branch that deletes migrations.rs).

Branch the BODY (keep signature stable, return `FeatureUnavailable` when off):
- `finalised_source.rs:962` capability-dispatch `get_tx_out_set_info_accumulator`
  (this is the seam that stops the cfg cascade).

Leave UNTOUCHED (compile as-is, error propagates): `chain_index.rs:2356` NFS fold,
`backends/{fetch,state}.rs`, `indexer.rs` trait, `zaino-serve service.rs` handler.

Cargo wiring (mirror `transparent_address_history_experimental`):
- `zaino-state/Cargo.toml`: `gettxoutsetinfo = []`; `default = []` unchanged. NOT under
  the `experimental_features` umbrella.
- `zaino-serve/Cargo.toml`: `gettxoutsetinfo = ["zaino-state/gettxoutsetinfo"]`.
- `zainod/Cargo.toml`: `gettxoutsetinfo = ["zaino-state/gettxoutsetinfo", "zaino-serve/gettxoutsetinfo"]`.

Tests/CI:
- Move accumulator tests under `#[cfg(feature = "gettxoutsetinfo")]`.
- Add one off-state test: RPC returns the typed error; no accumulator table created.
- CI matrix must build+test BOTH states (default off, and `--features gettxoutsetinfo`).

## Already edited this session
- `…/finalised_state/CONTEXT.md`: refined the **Schema hash** glossary entry to say it
  fingerprints the layout *description / format-if-present contract*, NOT physical table
  presence (feature-gated tables are described-but-optional).
- (Earlier, separate thread) `docs/notes/txout-set-accumulator.md` + EXP-0001 fix #1 (RAM
  dimension on the rebuild pre-flight gate). See `project_accumulator_quiz.md`.
