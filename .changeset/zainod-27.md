---
default: minor
---

# `zainod`: `[grpc]` `max_connection_age_secs` (default 1800, 0 = never), `max_connection_age_grace_secs` (600), …

`[grpc]` `max_connection_age_secs` (default 1800, 0 = never), `max_connection_age_grace_secs` (600), `max_connection_idle_secs` (1800, 0 = never) and `adaptive_streams` (default true): long-held connections are GOAWAY'd so clients re-resolve DNS, and a saturated node refuses new work within seconds. `/statusz` adds `load` = `{limit, in_flight, refused_per_s}` for the work pool.
