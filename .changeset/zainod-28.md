---
default: minor
---

# `zainod`: The gRPC listener serves public health: `GET /readyz` (`{"state", "detail"}`, `200` only when serving) and …

The gRPC listener serves public health: `GET /readyz` (`{"state", "detail"}`, `200` only when serving) and `GET /status` (state, detail, progress, notes, version, network, tip, served, validator counts, enabled indexes, egress Mbit/s per closed UTC 10-min window, `GetLightdInfo` fields; no addresses, peers, disk or mempool), over HTTPS/1.1 or h2 on the gRPC port. A node refusing ≥ 10% of gRPC work for 5 min reports `down` / `Overloaded` until 5 min under 2%; `/statusz` `load` adds `overloaded`.
