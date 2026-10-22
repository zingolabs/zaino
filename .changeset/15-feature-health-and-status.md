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
- `GET /status`: `{state, detail, chain, build, indexes}`. `chain` = network, best tip, served
  height, trusted / answering validator counts, alarms; `build` = version; `indexes` = each
  configured index → the RPCs it gates. No addresses, peers, disk or mempool.

`state` is one of, each with its own `detail`:

| State          | Meaning                                                                   | `detail`                                              |
| -------------- | ------------------------------------------------------------------------- | ----------------------------------------------------- |
| `provisioning` | starting: index-snapshot download, or plain startup                       | snapshot `{phase, bytes, total_bytes, bytes_per_sec}` |
| `syncing`      | indexes or the served tip behind the network's tip                        | `{height, target, blocks_per_sec}`                    |
| `serving`      | a trusted validator holds the served tip                                  | egress, last closed 10-minute window `{start, mbps}`  |
| `degraded`     | `[p2p]` peers carry a fresh tip that no trusted validator vouches for     | `{reason, message}`                                   |
| `held`         | operator hold (`zainod hold`)                                             | `{reason, message}`                                   |
| `down`         | no trusted validator reachable, tip not held, runtime stalled, overloaded | `{reason, message}`                                   |

A node persistently short of capacity reports `down` / `overloaded` (`Overloaded: <cause>`:
network saturated, CPU or IO pressure, refusing work) until it recovers. Under systemd `Type=notify`, zainod sends `READY=1` the first time
`/readyz` passes (or the node is held), and extends the start timeout while a sync or snapshot
download makes progress.
