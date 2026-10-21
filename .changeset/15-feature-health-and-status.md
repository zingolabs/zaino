---
default: minor
---

# `zainod`: readiness and status endpoints

Admin listener (`[metrics] listen_address`, unset = off), on its own thread and runtime:

- `/metrics`: Prometheus exposition.
- `/livez`: fails once the serving runtime has missed its heartbeat for 30 s.
- `/readyz`: `{"state", "detail"}`, `200` only when `serving` or `degraded`.

Public endpoints on the gRPC listener (same port and TLS, HTTP/1.1 or h2), safe to expose:

- `GET /readyz`: same body as the admin `/readyz`.
- `GET /status`: state, detail, progress, notes, version, network, best tip, served height,
  configured and connected validator counts, enabled indexes, egress per closed 10-minute
  window, and the `GetLightdInfo` fields. No addresses, peers, disk or mempool.

`state` is one of:

| State | Meaning |
| --- | --- |
| `provisioning` | starting: index-snapshot download, or waiting for a first validator answer |
| `syncing` | indexes or the served tip behind the network's tip (`progress` = `{done, total, unit, rate}`) |
| `serving` | a trusted validator holds the served tip |
| `degraded` | `[p2p]` peers carry a fresh tip that no trusted validator vouches for |
| `held` | operator hold (`zainod hold`) |
| `down` | `detail` says why: no trusted validator reachable, tip not held, runtime stalled, overloaded |

A node persistently short of capacity reports `down` / `Overloaded: <cause>` (network
saturated, CPU or IO pressure, refusing work) until it recovers. Under systemd `Type=notify`, zainod sends `READY=1` the first time
`/readyz` passes (or the node is held), and extends the start timeout while a sync or snapshot
download makes progress.
