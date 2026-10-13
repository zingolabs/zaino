---
default: major
---

# `zainod`: `/statusz`, `/readyz` and the `/metrics` gauges each read one load of the global snapshot

`/statusz`, `/readyz` and the `/metrics` gauges each read one load of the global snapshot. `/statusz` = `version`, `network`, `uptime_s`, `ready`, `reasons`, `grpc.sent_bytes`, `disk` (per index: `size_bytes`, `tables`) beside the snapshot's report: `seq`, `tips` (`best`, `final`, `served` as `{height, hash}`, `held_by`, `configured`, `synced`), `unready`, `handed`, `indexes` (`name`, `enabled` = configured, `durable`; listed from startup, before boot too), `validators` (now with `state`, `latency_ms`, `failures` from the traffic balancer), `alarms` (now with `finality_paused`), `mempool`, `forks`. Gone: `tip`, `best_height`, `fetch_height`, `served_height`, top-level `synced`, and `size_bytes` / `tables` inside `indexes`. `/readyz` reasons and every ztest metric family are unchanged. Alarm edge logs and `Serving the verified tip` now log under `Snapshot`; one progress task logs `Syncing blocks` and each index's `Syncing` (the per-index `Serving` line is gone).

**Migration:** Status readers: `tip.height` → `tips.best.height` (held: `tips.held_by > 0`), `best_height` → `tips.best.height`, `fetch_height` → `handed`, `served_height` → `tips.served.height`, `synced` → `tips.synced`, `indexes[].size_bytes` / `tables` → `disk.<index>`.
