---
default: major
---

# `zaino-chain-head-service`: The `prometheus` feature and the implicit `metrics` feature are removed; reorg metrics are always emitted and …

The `prometheus` feature and the implicit `metrics` feature are removed; reorg metrics are always emitted and cost nothing without an installed recorder.

**Migration:** Remove `prometheus` or `metrics` from this crate's feature list in dependent manifests.
