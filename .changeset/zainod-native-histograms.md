---
default: major
---

# `zainod`: every histogram is a Prometheus native histogram, served over protobuf

`/metrics` negotiates its format from `Accept`: protobuf when the scraper offers
`application/vnd.google.protobuf`, else text. Histograms are native (exponential buckets about
9% wide, rescaling past 160 per series), and the text format cannot carry them:

- A text scrape keeps every counter and gauge but has no histogram; `*_bucket` series are gone.
- Prometheus: set `scrape_native_histograms: true` on the zainod job (it then negotiates
  protobuf). Alloy: `scrape_native_histograms = true` on `prometheus.scrape`, and
  `send_native_histograms = true` on each `prometheus.remote_write` endpoint.
- Queries drop `_bucket` and `le`:
  `histogram_quantile(0.95, sum by (method) (rate(zaino_grpc_duration_seconds[5m])))`.
- zebra-network's `zcash_net_peer_handshake_duration_seconds` is a native histogram too (was a
  summary).

Library crates: `METRIC_BUCKETS` is gone from `zaino-grpc`, `zaino-source`, `zaino-chainview`,
`zaino-persistence` and `zaino-sync` (no bucket edges to configure).
