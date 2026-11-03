---
default: minor
---

# `zainod`: `[grpc]` serving limits and load shedding

Every cap refuses rather than queues (`UNAVAILABLE` with a retry hint), except the read lanes,
which wait. Defaults shown:

| Key | Default | Bounds |
| --- | --- | --- |
| `max_connections` | 4096 | open connections (+ 1024 must fit the open-file limit, or boot fails) |
| `max_connections_per_ip` | 32 | per client: the peer, or the address a trusted proxy names |
| `max_streams_per_connection` | 8 | h2 streams on one connection |
| `max_work_streams` | 2048 | streams of every method but `GetMempoolStream` |
| `adaptive_work_streams` | `true` | admitted work follows measured latency, `max_work_streams` the ceiling |
| `max_subscriptions` | 4096 | `GetMempoolStream` subscriptions |
| `max_point_reads` / `max_range_reads` / `max_scan_reads` | 64 / 32 / 4 | index reads in flight per lane |
| `unread_timeout_secs` | 300 | data the client leaves unread → connection closed |
| `silent_timeout_secs` | 300 | nothing to send → stream ends `UNAVAILABLE` (`GetMempoolStream` exempt) |
| `max_connection_age_secs` | 1800 | GOAWAY so clients re-resolve DNS (±10%; 0 = never) |
| `max_connection_age_grace_secs` | 600 | in-flight streams of a GOAWAY'd connection |
| `max_connection_idle_secs` | 1800 | GOAWAY a connection with no open stream (0 = never) |
| `trusted_proxies` | `[]` | proxies that send a PROXY protocol v1/v2 header |
| `shutdown_grace_secs` | 10 | open connections' time to finish after SIGTERM / SIGINT |

- `[grpc.overload]` latches `/readyz` `down` / `Overloaded: <cause>` on `CPU pressure` or
  `IO pressure` (PSI, 25%), or `refusing work`. `zainod overload on|off|set|unset` overrides the detector across
  restarts, as `zainod hold` does.
- `[serve.tls]` certificates are re-read when they change: renewal needs no restart.
- Streamed answers pack ready messages into full 16 KiB HTTP/2 DATA frames, so h2 0.4.16+ clients
  that read late no longer hit `GOAWAY too_many_data_frames`.
