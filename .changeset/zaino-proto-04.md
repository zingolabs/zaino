---
default: major
---

# `zaino-proto`: Bindings generated into `OUT_DIR` at build; `protoc` required; no `CompactTxStreamer` server stub

Bindings are generated into `OUT_DIR` on every build and nothing generated is committed, so building needs `protoc` (on `PATH` or named by `PROTOC`). `proto::service` no longer has `compact_tx_streamer_server` (zaino-grpc dispatches by request path); the client and every message are unchanged, and `proto::zebra_indexer` keeps its server half.
