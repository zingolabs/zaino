---
default: major
---

# `zaino-source`: `zaino_validator_rpc_failures_total{failure}` is a bounded set

A JSON-RPC error code the client reads no verdict from is `rpc_other` (was `rpc_{code}` for any
code), and an HTTP status is its class, `http_3xx` / `http_4xx` / `http_5xx` (was `http_{status}`).
Queries on `failure="http_503"` or an unread code such as `failure="rpc_-28"` now match nothing.
