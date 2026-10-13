---
default: major
---

# `zaino-grpc`: Load shedding

Load shedding. `GrpcLimits` gains `max_connection_age` (default 30 min, ±10% per connection), `max_connection_age_grace` (600 s) and `max_connection_idle` (30 min): an aged or idle connection gets a GOAWAY and its in-flight streams the grace (gRPC A9; `zaino_grpc_retired_connections_total{reason}`). `adaptive_streams` (default on) caps work streams by measured server time per message (Gradient2, `max_streams` the ceiling, floor 32, starting 256), refusing past it with the same `UNAVAILABLE` + pushback (`zaino_grpc_adaptive_stream_limit`). `GrpcService::load()` → `Load::report()` = `{limit, in_flight, refused_per_s}`.

**Migration:** A `GrpcLimits` literal names the four new fields (or takes `..GrpcLimits::default()`).
