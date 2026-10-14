---
default: major
---

# `zaino-persistence`: `zaino_lsm_*` series are labelled `index` and `table`

Every `zaino_lsm_*` series and LSM log line carries `index` (the `IndexKind` name, as the store metrics do) and `table` (the map's name) in place of the bare `set` label, so two indexes' same-named tables never share a series and each series joins to its index. Metric names are unchanged.
