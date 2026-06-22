# Zaino

Zaino is a Zcash indexer that serves wallet/RPC traffic from its own validated
view of the chain, backed by a validator (zebra, or — being deprecated — zcashd).

## Language

### Chain state

**Finalization ceiling**:
The height `chain_tip − NON_FINALIZED_DEPTH`. At or below it a block is
*finalized* — immutable, reorg-safe to fetch from the validator by height. Above
it is the reorg-mutable non-finalized window. The value tracks the chain tip, so
it can move *backwards* after a chain-shortening reorg (see zaino#1128); it is
not monotonic.
_Avoid_: finalized height floor, NFS floor, anchor height — for the boundary
*value*. (The code function is `finalization_ceiling`, matching the
`reify_NFS_when_FS_synced` draft.)

**Non-finalised state (NFS)**:
Zaino's validator-sourced view of the reorg-mutable window `[ceiling, tip]`. The
NFS *leads* the finalised DB and never waits for it to catch up.
_Avoid_: non-finalized cache, mempool (unrelated).

**NFS anchor (seam block)**:
The block at the finalization ceiling that roots the non-finalized window. It is
served from the finalised DB when that DB has reached the ceiling, otherwise from
the validator directly. The anchor is defined by the ceiling height alone — *not*
by wherever the finalised DB tip currently sits.
_Avoid_: root block, genesis seed.

**Finalised state / finalised tip**:
The durable on-disk index of finalized blocks; the *finalised tip* is its highest
stored height (`db_height`). It lags the finalization ceiling during background
catch-up and equals it in steady state — it never determines the NFS anchor.
_Avoid_: finalized database height (when referring to the tip value).

**Provisional**:
The condition where the finalised DB has not yet caught up to the NFS, so a height
in `[finalised_tip, ceiling]` is served via the validator passthrough rather than
from the durable index (see zaino#1096).
