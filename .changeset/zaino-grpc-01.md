---
default: minor
---

# `zaino-grpc`: First release

First release. The wallet-facing lightwalletd `CompactTxStreamer` server: one `GrpcService`, every method dispatched by path over `Routes { chain, validators, network, nfs, max_address_rows }`. Every index method reads one `zaino_nfs` snapshot for its whole request or stream; a disabled index answers `UNIMPLEMENTED`, no snapshot yet `UNAVAILABLE`. Mempool methods, `GetLightdInfo` and `SendTransaction` go through the chain view, `GetTransaction` through the validators' `TrafficBalancer`. `GrpcLimits` bounds connections (also per client), streams, `GetMempoolStream` subscriptions, point / range / scan read lanes, stalled connections and request bodies, and sets the shutdown drain (GOAWAY, `drain_timeout`, default 10 s); `with_trusted_proxies(TrustedProxies)` reads PROXY protocol v1/v2 headers; `with_tls` serves TLS from PEM files re-read on change.
