# Finalised Index Append-Only Design

## Context

Zaino's chain index is split into two state layers:

- the **non-finalised state (NFS)** — the recent, reorg-prone tip; it owns all
  reorg handling and absorbs the churn of competing chain tips; and
- the **finalised state (FS)** — the durable, LMDB-backed index of blocks deep
  enough to be treated as settled.

While optimising the forward sync path (eliminating the random B+tree page
faults that collapse the sync rate once the indexes exceed RAM), one question
kept recurring: does the finalised index need a path that takes committed state
and produces the precise *prior* committed state in place? Such a precise
inverse is expensive on the write-hot path — it forces cold, value-resolving
database reads (`load_prior_transactions`, `calculate_..._after_delete_block`)
to survive on helpers shared with the forward path, and it blocks moving the
forward path's derived lookups into memory.

This record settles that question for all current and future finalised-index
work.

## Decision

**The finalised index is append-only or restored-from-checkpoint. It is never
incrementally rolled back.**

Finalised state only ever moves *forward* — new blocks appended in strict
height order — or is *discarded back to a known-good committed point and
re-derived forward*. There is no supported operation that consumes committed
finalised state and yields the precise prior committed state in place.

## How backward movement is handled

Exactly three events could move the finalised tip backward. None is an in-place
inverse:

1. **Reorg.** Handled entirely within the NFS. A reorg never reaches the
   finalised DB — by construction, only blocks beyond the reorg horizon
   (`MAX_NONFINALIZED_DEPTH`) are finalised.
2. **Non-catastrophic rollback.** Restore to a tracked checkpoint and resume
   forward sync from there. Zaino tracks a checkpoint vector at roughly 24, 48,
   72, and 96 hours behind the tip; a rollback selects the deepest checkpoint
   that precedes the divergence and forward-syncs from it.
3. **Catastrophic rollback** (deeper than the oldest checkpoint). A full sync
   from scratch (genesis) is always available.

## Consequences

- **Reset is re-derive-forward, never reverse.** Every backward transition is
  "discard to a known-good committed point, then append forward." No code path
  resolves a committed output's prior value in order to undo it in place.

- **Derived in-memory state reconstructs from committed state on open.** A
  derived cache is reseeded by scanning the committed tables, not by inverting
  the last block. This is why reseed-on-open — not a precise `apply_reverse` —
  is the correct maintenance model for derived caches: it is the *same*
  primitive a checkpoint restore relies on. The in-memory transparent UTXO
  cache (`db/v1/utxo_cache.rs`) follows this directly: it seeds from committed
  `transparent − spent`, and a block delete reseeds rather than value-inverting.

- **`delete_block` is failed-write cleanup, not rollback.** Its only live
  driver is wiping a block whose append failed before it durably committed —
  restoring the last-good append point. It is not, and must not become, a reorg
  or rollback mechanism. Any future need to "move the tip back" is a checkpoint
  restore, not a `delete_block` loop.

- **The reverse accumulator is scoped to that cleanup.** The reverse
  txout-set-accumulator computation
  (`calculate_tx_out_set_info_accumulator_after_delete_block`) and the cold
  database reads it needs are justified *only* by failed-write cleanup. They
  must never be relied on by the forward write path, and the forward path must
  not be refactored to share their read machinery.

## Cross references

- Forward-path read elimination and the in-memory transparent UTXO cache:
  `packages/zaino-state/src/chain_index/finalised_state/db/v1/utxo_cache.rs`.
- Append-only and tip-only delete contracts on the write surface:
  `packages/zaino-state/src/chain_index/finalised_state.rs` (`write_block`,
  `delete_block_at_height`, `delete_block`).
- NFS boundary: `MAX_NONFINALIZED_DEPTH` bounds the non-finalised layer;
  rollbacks shallower than it are NFS-internal, deeper ones are checkpoint
  restores per this record.
