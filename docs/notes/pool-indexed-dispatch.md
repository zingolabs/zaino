# Follow-up: pool-indexed dispatch in the chain index

Status: proposed (not started), tracked in
[#1367](https://github.com/zingolabs/zaino/issues/1367). Scope: `zaino-state`
chain index. Origin: the PR #1362 (Ironwood/NU6.3) review.

## Problem

Ironwood was added to the chain index by copy-paste: every place that handled
"sapling and orchard" gained a third clone. The review of that PR found six
bugs, and every one of them was a copy-paste artifact of exactly this shape:

- a sapling-output skip reusing the orchard-refactor's spend-width variable;
- the ironwood JSON field reading the `"orchard"` key;
- an error message describing the ironwood pool as "active NU5";
- per-pool activation gating re-implemented (and drifted) at three sites;
- per-pool tuples widened positionally, letting same-typed roots transpose
  silently.

The review's DRY pass consolidated the *implementations* (shared cursor walks,
`required_pool_root`/`optional_pool_root`, `extract_block_pool_lists`,
`BlockRowEntries`), so each per-pool behaviour now has one definition. What it
did not change is the *dispatch*: pools are still reached by field name
(`self.sapling`, `tx.ironwood()`, `treestate.orchard`), so adding the next
pool still means hand-editing every call site, and the compiler cannot point
at the sites that were missed.

## Proposal

Make `ShieldedPool` (already defined in `chain_index.rs`) the index for
per-pool data and behaviour:

1. **Table dispatch**: `DbV1::pool_table(&self, ShieldedPool) -> lmdb::Database`
   replacing direct field access in the pool read/write paths, plus a
   `MissingRow` policy per pool (`ShieldedPool::missing_row_policy()` — dense
   for sapling/orchard, sparse for ironwood and later pools).
2. **Activation dispatch**: `ShieldedPool::activation_upgrade() ->
   NetworkUpgrade` (Sapling → Sapling, Orchard → Nu5, Ironwood → Nu6_3),
   consumed by `required_pool_root`/`optional_pool_root` callers instead of
   per-site `is_nu_active` triples.
3. **Per-transaction data dispatch**: a method on the indexed transaction type
   returning the pool's compact data by `ShieldedPool` value, so
   `extract_block_pool_lists` and the compact-block builders iterate pools
   instead of naming them.
4. **Exhaustiveness as the safety net**: every per-pool `match` on
   `ShieldedPool` (no wildcard arms). Adding the NU7 pool then fails
   compilation at every site that needs a decision — the same mechanism that
   surfaced the `PoolType::Shielded(ShieldedPool::Ironwood)` non-exhaustive
   match errors in the devtool work.

## Constraints and cautions

- **Behaviour preserving.** No schema, wire, or semantics change; this is a
  dispatch refactor over the already-consolidated helpers.
- **Type asymmetry is real**: sapling has spends+outputs and its own types;
  orchard and ironwood share the Orchard compact types. The dispatch layer
  must not force a premature unification of the per-pool value types —
  associated types or per-pool `match` arms returning distinct types are
  acceptable; a trait object is not required.
- **Overlap warning**: this touches the same table-handle layer as the paused
  DB/wire orthogonalization work (branch `clean_block_id_ident_relation`).
  Reconcile with that plan before starting; doing both independently will
  conflict.
- The lightwalletd protocol side (proto field per pool) stays copy-per-pool by
  nature of protobuf; this proposal covers the indexer internals only.

## Suggested shape of the work

One branch, roughly three commits: (1) `ShieldedPool` gains
`activation_upgrade` / `missing_row_policy` and the connector/finalised-state
call sites consume them; (2) `DbV1::pool_table` + pool-iterating read/write
paths; (3) per-transaction pool-data dispatch and compact-block builders.
Each commit behaviour-preserving with the full `zaino-state` suite green.
