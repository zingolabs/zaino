---
default: minor
---

# `zainod`: `[grpc] shutdown_grace_secs` (default 10): on SIGTERM/SIGINT the gRPC listener closes at once and open …

`[grpc] shutdown_grace_secs` (default 10): on SIGTERM/SIGINT the gRPC listener closes at once and open connections get that long to finish (GOAWAY sent); out of a load balancer's rotation first = `zainod hold`. Under systemd `Type=notify` zainod sends `READY=1` once `/readyz` first passes (or the node is held), and start-timeout extensions while a sync or snapshot bootstrap progresses; nothing after `READY=1`.
