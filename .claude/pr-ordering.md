# PR ordering for sync/observability work

Each branch is based on the previous one. PRs should merge in order.

1. **`feature/sync-batching-and-status-fix`** → dev
   - Remove per-block status transitions from write_core
   - Batch sync loop: 10k blocks per iteration
   - NFS gap guard

2. **`feature/sync-loop-tracing-spans`** → depends on #1
   - Per-iteration tracing spans (replaces infinite #[instrument] span)

3. **`feature/otel-integration`** → depends on #2
   - OTEL deps (tracing-opentelemetry, opentelemetry, opentelemetry-otlp)
   - Feature-gated `otel` in zaino-common + zainod
   - init_otel_provider() in logging.rs
   - Spans actually export to Tempo/Jaeger
   - `operator_otel` bundle feature

4. **`feature/instrument-hot-path`** → depends on #3
   - #[instrument] on sync_to_height, write_block, DbV1::write_block, handle_reorg, send_request

5. **`trial/prometheus-metrics`** → merged into dev (PR #1216)
   - Prometheus /metrics endpoint, feature-gated behind `prometheus`

## Branch status

| # | Branch | Commits | Status |
|---|--------|---------|--------|
| 1 | feature/sync-batching-and-status-fix | 3 | pushed |
| 2 | feature/sync-loop-tracing-spans | 1 (on top of #1) | pushed |
| 3 | feature/otel-integration | 2 (on top of #2) | pushed |
| 4 | feature/instrument-hot-path | 1 (on top of #3) | pushed |
| 5 | trial/prometheus-metrics | — | merged (PR #1216) |
