---
default: patch
---

# `zaino-peers`: peer latency = answered round trips only

`zaino_peers_request_duration_seconds` times a request from its send to a ready peer to that
peer's answer. Failures (zebra's local `NoReadyPeers` / synthetic `notfound`, timeouts), the wait
for a ready peer and pushes (answered `Nil` by zebra's fan-out, never by a peer) are no longer in
it; failures stay counted in `zaino_peers_request_failures_total`.
