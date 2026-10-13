---
default: patch
---

# `zaino-chain-head-service`: `zaino.sync.reorg_total` / `reorg_depth` now count reorgs won by a longer chain (previously missed by a …

`zaino.sync.reorg_total` / `reorg_depth` now count reorgs won by a longer chain (previously missed by a height comparison); depth = blocks rewritten back to the fork point. An old tip that an advance pushed below the retained window, or that a re-anchor dropped, is an advance and not a reorg; only an old tip still inside the window counts as a reorg of unknown depth. Feature `prometheus` removed; emission is unconditional.
