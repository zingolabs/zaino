---
default: minor
---

# `zaino-sync`: Index progress comes from each writer, not the served snapshot: `zaino_index_applied_height`, …

Index progress comes from each writer, not the served snapshot: `zaino_index_applied_height`, `zaino_index_applied_blocks_total` and `zaino_index_applied_rows_total` as blocks are applied, `zaino_index_finalized_height` = the committed tip (moved from zaino-snapshot), all `{index}`; `IndexHandle::applied()`.
