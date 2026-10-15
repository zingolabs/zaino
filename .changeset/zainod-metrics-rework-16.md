---
default: major
---

# `zainod`: admin `/statusz` removed; the public `/status` on the gRPC listener is the one status body

- Admin listener serves `/metrics`, `/livez`, `/readyz` only
- Readiness, progress and notes: public `/status`; numbers: `/metrics`
- `zaino-snapshot::Report` shrinks to what `/status` and the gauges read
