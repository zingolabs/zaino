---
default: major
---

# `zaino-source`: The transport makes one attempt per call; concurrency, request rate, retries and failover are …

The transport makes one attempt per call; concurrency, request rate, retries and failover are `zaino-traffic`'s. Removed: `TrafficBalancer`, `Lane` and `ZebraRpcAdapter::{new, on}`, `RpcClientConfig::{max_retries, retry_delay}` (a work-queue-full batch item is now that item's own refusal), `LinkLimits::{new, MIN_CONNECTIONS, max_requests_per_sec, max_connections()}`, `FailureMode::is_transient`, and the `zaino.validator_rpc.wait_seconds` and `zaino.validator_rpc.retries_total` metrics (`in_flight` loses its `lane` label). `LinkLimits` is now `{ max_connections, max_bytes_per_sec }`; `RpcClient::call(method, params)` takes no lane.

**Migration:** Build each `ZebraRpcAdapter` with `at(.., LinkLimits { max_connections, max_bytes_per_sec })` and hand it to `zaino_traffic::TrafficBalancer` (`Trusted { source, priority, limits }`, the request rate in `zaino_traffic::Limits`); ask through the balancer instead of `failover` or a lane handle.
