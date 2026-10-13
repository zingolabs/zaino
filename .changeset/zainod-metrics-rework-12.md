---
default: major
---

# `zaino-rpc`: `RpcClient::call` / `call_with_timeout` take `method: &'static str` (bounds metric-label cardinality)

`RpcClient::call` / `call_with_timeout` take `method: &'static str` (bounds metric-label cardinality). Outbound metrics are now `zaino.rpc.outbound.duration_seconds{method}` and `zaino.rpc.outbound.errors_total{method,outcome}` (`transport_error`, `rpc_error`, `retried`).

**Migration:** Pass method names as string literals. Removed metrics: `zaino.rpc.outbound.requests_total`, `request_duration_seconds`, `retries_total`. Feature `prometheus` removed.
