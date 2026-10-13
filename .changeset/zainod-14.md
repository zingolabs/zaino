---
default: minor
---

# `zainod`: gRPC serving limits under `[grpc]`: `max_connections`, `max_connections_per_ip`, …

gRPC serving limits under `[grpc]`: `max_connections`, `max_connections_per_ip`, `max_streams_per_connection`, `max_streams`, `max_subscriptions` (`GetMempoolStream`, counted apart), read lanes `max_point_reads` / `max_range_reads` / `max_scan_reads`, `stall_timeout_secs` and `trusted_proxies` (PROXY protocol v1/v2). `[serve] max_address_rows` (100,000) bounds one address request (`RESOURCE_EXHAUSTED` over it). Request bodies are capped (64 KiB; 2 MB + 1 KiB for `SendTransaction`). TLS certificates are re-read on change.
