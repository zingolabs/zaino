---
default: minor
---

# `zaino-grpc`: `GrpcService::retire()` → `Retire::retire_all()`: every connection open at that moment is sent GOAWAY, its …

`GrpcService::retire()` → `Retire::retire_all()`: every connection open at that moment is sent GOAWAY, its in-flight streams given `max_connection_age_grace` (`zaino_grpc_retired_connections_total{reason="retired"}`); later connections are untouched.
