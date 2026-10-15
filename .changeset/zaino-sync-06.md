---
default: major
---

# `zaino-sync`: `IndexHandle::durable_height()` / `applied_height()` replace `tip()` / `applied()`

`IndexHandle::tip()` (the durable tip) and `applied()` (the last block folded) are now `durable_height()` and `applied_height()`, both `Option<BlockHeight>`. A caller that needs the durable tip's hash reads `view().tip()`. No behaviour change.
