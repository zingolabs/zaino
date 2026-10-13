---
default: major
---

# `zaino-state`: Sync-loop metrics: added `zaino.sync.consecutive_failures` and `zaino.sync.backoff_seconds`

Sync-loop metrics: added `zaino.sync.consecutive_failures` and `zaino.sync.backoff_seconds`.

**Migration:** Removed metrics: `zaino.sync.lag_blocks` (derive `zaino.chain.tip_height - zaino.sync.finalized_height`), `iterations_total`, `iteration_duration_seconds`, `errors_total`, `has_reached_tip`, `reached_tip_at`. Feature `prometheus` removed.
