---
default: minor
---

# `zaino-snapshot`: The snapshot is the one reporting source: `Report::of(snap, &SyncProgress, &MemberTable, enabled)` lists …

The snapshot is the one reporting source: `Report::of(snap, &SyncProgress, &MemberTable, enabled)` lists every foldable index as configured (`indexes(enabled, durable)` builds the same list before a snapshot exists) and joins each validator's facts with the traffic balancer's row (`state`, `latency_ms`, `failures`) and reports `finality_paused`; `emit_gauges(snap, &NfsProgress)` also sets every `zaino_chainview_*` state gauge at scrape; the publisher logs each chain view alarm's rise and clear (stale tip, end of service, partition, eclipse, finality paused). `Unready::label` = the `/readyz` text. Feature `testing`: `Snapshots::fixed(indexed, view)`.
