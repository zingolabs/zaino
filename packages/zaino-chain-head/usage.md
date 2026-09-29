# `zaino-chain-head` — usage

The non-final window of the best chain, held in memory as `Arc<Block>`s: what
`zaino-sync`'s `Producer` follows the quorum tip with, and replays a reorg from
without fetching again. A library, not a task: its one caller drives it.

```rust,ignore
use zaino_chain_head::{Advance, ChainHead};

let mut head = ChainHead::new(anchor, depth); // anchor: Arc<Block>, depth: finalised depth

match head.advance(quorum_tip, &pool).await? { // quorum_tip: BlockRef, pool: &BlockFetchPool<S>
    Advance::Unchanged => {}
    Advance::Extended => for block in head.best_chain_from(next) { /* add */ },
    Advance::Reorg { fork } => { /* reset, then best_chain_from(resume) */ }
}
```

## Public surface

| item | role |
|---|---|
| `ChainHead::new(anchor, depth)` | a window of one block; `depth` > 0 |
| `advance(tip, &pool)` | follow `tip`, fetching what the window lacks |
| `best_chain_from(height)` | canonical blocks `height ..= tip`; `height` ≥ `floor()` |
| `tip()` / `floor()` | ends of the window |
| `Advance` | `Unchanged`, `Extended`, `Reorg { fork }` (heights ≥ `fork` replaced, or dropped on a retreat) |
| `AdvanceError` | `FetchHeight` / `FetchHash` (validators failed after retries), `BelowWindow` |

## Advance rules

- `tip` = the held tip → `Unchanged`
- `tip` = a held ancestor → a retreat: window cut back onto it, `Reorg { fork:
  tip + 1 }` with nothing new to read (onto the floor itself included)
- clean extension → one concurrent fetch of `ours + 1 ..= tip` by height through
  the pool, kept only if it links onto the held tip and ends on `tip`'s hash
- anything else → walk back by `prev_hash` from `tip`'s hash (`block_by_hash`,
  primary validator first) to the window block it links onto; that parent being
  the held tip is `Extended`, anything lower is `Reorg`
- a walk that passes below the floor → `BelowWindow`, window untouched (the
  fork is past the consensus reorg bound, so final data is wrong)
- `tip` must be at most `depth` above the held tip (panics otherwise): the
  caller bulk-fetches wider gaps

## Window bounds

The floor is `highest tip seen − depth`, the sink's final boundary − 1, so
every fork a reorg can legally make is inside the window, and a reset's resume
height always is too. A lower winning tip does not lower the floor.

The trim to that floor runs at the **start** of the next `advance`, never at
the end of the one that raised the tip: after a reorg onto a higher tip, the
producer still replays the new branch from the resume height out of the
window, and an eager trim would drop blocks it has not yet published. So the
window holds at most `2 × depth + 1` blocks between advances, and is asserted
hash-linked, gapless and within that bound after every advance.
