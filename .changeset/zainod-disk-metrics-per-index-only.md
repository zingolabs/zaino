---
default: major
---

# `zainod`: disk metrics per index only

- **Removed:** `zaino_disk_free_bytes{index}` and `zaino_disk_capacity_bytes{index}` (one
  filesystem repeated per index label, over-counted when summed) and
  `zaino_disk_{read,written}_bytes_total` (process block-device I/O). Read filesystem space and
  I/O from the node exporter or cAdvisor.
- **Kept:** `zaino_disk_committed_bytes{index}`, `zaino_disk_cached_bytes{index}`,
  `zaino_disk_bytes{index}`.
