---
default: minor
---

# `zaino-persistence`: Every `zaino_lsm_*` series of a map is published from open, at zero until its first batch, merge or stall …

Every `zaino_lsm_*` series of a map is published from open, at zero until its first batch, merge or stall (`zaino_lsm_merge_duration_seconds` at `tier="0"`), so an absent series means a build without it.
