---
default: major
---

# `zaino-serve`: Removed `zaino.grpc.requests_total` (= `zaino.grpc.request_duration_seconds` `_count`); …

Removed `zaino.grpc.requests_total` (= `zaino.grpc.request_duration_seconds` `_count`); `zaino.grpc.errors_total{code}` now carries the status name (`NotFound`). Feature `prometheus` removed.

**Migration:** Use `zaino.grpc.request_duration_seconds_count` for request volume.
