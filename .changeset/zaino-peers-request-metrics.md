---
default: minor
---

# `zaino-peers`: peer request latency and failures

`zaino_peers_request_duration_seconds{request}` (native histogram) and
`zaino_peers_request_failures_total{request, failure}` time and count every request zaino makes
of the peer set: header and block fetches, mempool listings, transaction fetches, pushes.
`request` is zebra's command name, the same label as zebra-network's message counters.
`zaino_peers::describe_metrics` registers them.
