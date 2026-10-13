---
default: patch
---

# `zaino-grpc`: `SendTransaction` rejections answer like lightwalletd: gRPC `OK` with the validator's own JSON-RPC code and …

`SendTransaction` rejections answer like lightwalletd: gRPC `OK` with the validator's own JSON-RPC code and message (`-22` for unparseable bytes), not `-1` and a reworded reason. zebrad's "transaction already exists in mempool" and "already queued for download" (code `-1`) now reach the wallet as rejections instead of `UNAVAILABLE`.
