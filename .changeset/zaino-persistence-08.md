---
default: minor
---

# `zaino-persistence`: `DiskView::footprint()` → `TableFootprint { table, bytes, cached }` per table (committed bytes, page-cache …

`DiskView::footprint()` → `TableFootprint { table, bytes, cached }` per table (committed bytes, page-cache share by `mincore`, `None` when not knowable). Each commit records `zaino_store_commit_seconds` + `zaino_store_commit_bytes_total` `{index}`; crate-level `describe_metrics` (store + LSM) and `METRIC_BUCKETS`.
