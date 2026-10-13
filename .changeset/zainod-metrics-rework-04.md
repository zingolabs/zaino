---
default: patch
---

# `zainod`: A `metrics_endpoint` configured on a binary built without `prometheus` now warns instead of being silently …

A `metrics_endpoint` configured on a binary built without `prometheus` now warns instead of being silently ignored; a non-private bind warns that the listener is unauthenticated.
