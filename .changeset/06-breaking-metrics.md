---
default: major
---

# `zainod`: new metric families; every 0.10 `zaino_*` / `zainod_*` metric is gone

Metrics are always compiled in (the `prometheus` feature is gone) and served on the admin
listener's `/metrics`. Dashboards and alerts need rewriting: only the `process_*` families carry
over.

| 0.10 metric | Nearest 0.11 metric |
| --- | --- |
| `zaino_chain_tip_height`, `zaino_sync_target_height` | `zaino_best_tip` |
| `zaino_sync_fetched_height` | `zaino_fetch_height` |
| `zaino_sync_finalized_height` | `zaino_index_finalized_height{index}` |
| `zaino_sync_fetched_*_total` | `zaino_fetch_*_total` |
| `zaino_sync_reorg_total` | `zaino_reorgs_total` |
| `zaino_grpc_request_duration_seconds` | `zaino_grpc_duration_seconds{method}` |
| `zaino_grpc_errors_total` | `zaino_grpc_requests_total{method, code}` |
| `zaino_rpc_outbound_duration_seconds`, `zaino_rpc_outbound_errors_total` | `zaino_validator_rpc_duration_seconds`, `zaino_validator_rpc_failures_total` |
| `zaino_mempool_transactions` | `zaino_chainview_mempool_transactions{state}` |
| `zainod_build_info` | `zaino_build_info` |

Removed with no replacement: `zaino_db_*`, `zaino_migration_*`, `zaino_jsonrpc_*`, the rest of
`zaino_sync_*` and `zaino_mempool_*`, and `zainod_restarts_total`.

0.11 families:

- **Chain and sync:** `zaino_best_tip`, `zaino_fetch_height`, `zaino_fetch_*_total` (blocks,
  transactions and per-pool work), `zaino_reorgs_total`, and per `{index}`:
  `zaino_index_synced`, `zaino_index_finalized_height`, `zaino_index_applied_height`,
  `zaino_index_applied_{blocks,rows}_total`, `zaino_index_{run,write}_seconds`.
- **Validators:** `zaino_validator_rpc_*`, `zaino_traffic_misanswers_total`, and the chain
  view's `zaino_chainview_*` (tip holders, agreement, validator state and release, push streams,
  peers, finality pause, mempool, submissions).
- **State:** `zaino_state{state}` and `zaino_state_reason{reason}`, one-hot over `/readyz`'s
  state and detail.
- **gRPC serving:** `zaino_grpc_*` per `{method}` (requests, one-reply and first-message
  latency, bytes sent), connection, admission, subscription and adaptive-limit gauges and
  counters, per-network `zaino_grpc_queue_delay_seconds` and `zaino_grpc_retransmit_ratio`, and
  `zaino_grpc_overload_latched{cause}`.
- **Peers** (`[p2p]` on): `zaino_peers_request_duration_seconds{request}`,
  `zaino_peers_request_failures_total{request, failure}`, and zebra-network's `zcash_net_peers`,
  `pool_num_{ready,unready}`, `zcash_net_{in,out}_messages{command}`,
  `zcash_net_{in,out}_bytes_total`, `zcash_net_connection_outcomes_total`.
- **Storage:** `zaino_store_commit_{seconds,bytes_total}{index}`, `zaino_lsm_*{index}`,
  `zaino_disk_{committed,cached}_bytes{index}`, `zaino_disk_bytes{index}` (sampled every 30 s).
- **Process:** `process_*` (CPU, resident memory, file descriptors, threads).

Every histogram is a Prometheus native histogram, so `/metrics` serves protobuf when the scraper
offers `application/vnd.google.protobuf` (a text scrape carries no histogram). Prometheus: set
`scrape_native_histograms: true` on the zainod job. Alloy: `scrape_native_histograms = true` on
`prometheus.scrape` and `send_native_histograms = true` on each `prometheus.remote_write`.
Queries drop `_bucket` and `le`:
`histogram_quantile(0.95, sum by (method) (rate(zaino_grpc_duration_seconds[5m])))`.
