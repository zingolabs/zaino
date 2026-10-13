---
default: minor
---

# `zainod`: Admin listener (`[metrics] listen_address`, its own thread and runtime): `/metrics`, `/livez` (fails once the …

Admin listener (`[metrics] listen_address`, its own thread and runtime): `/metrics`, `/livez` (fails once the serving runtime stops its heartbeat for 30 s), `/readyz` (`ready` plus reasons: `starting`, `heartbeat_stale`, `headers_syncing`, `tip_not_held`, `syncing`, the `snapshot_*` phases) and `/statusz` (verified tip and its holders, header-chain best, fetch and served heights, each validator, chain-view alarms, mempool spread, every index).
