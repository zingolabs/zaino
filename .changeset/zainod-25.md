---
default: minor
---

# `zainod`: Disk monitor, sampled every 30 s: `zaino_disk_committed_bytes` / `zaino_disk_cached_bytes` `{index, table}` …

Disk monitor, sampled every 30 s: `zaino_disk_committed_bytes` / `zaino_disk_cached_bytes` `{index, table}` (page-cache share of committed data), `zaino_disk_bytes`, `zaino_disk_free_bytes`, `zaino_disk_capacity_bytes` `{index}`, `zaino_disk_read_bytes_total` / `zaino_disk_written_bytes_total` (block-device bytes) and `zaino_io_stall_seconds_total{kind}` (own cgroup's I/O pressure). `rustix` is no longer feature-gated.
