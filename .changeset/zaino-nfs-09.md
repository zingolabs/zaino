---
default: major
---

# Workspace: names a reader does not trip on

- `zaino-nfs`: `Indexed<V>` → `NfsPublication<V>`, `At<V>` → `IndexesAtBlock<V>`.
- `zaino-grpc`: `Routes` → `RouteDeps` (the dependency bundle every route reads).
- `zaino-grpc-snapshot`: `GrpcSnapshot::view()` → `chain_view()`.
- `zaino-sync`: `IndexHandle::view()` → `durable_view()`; `Checked::at()` → `block_ref()`.
- `zaino-primitives`: `ReorgDepth` → `FinalityDepth`; `Block::at()` → `block_ref()`.
- `zaino-source`: `ZcashLegacyRpcClient::at`, `ZebraIndexerGrpcClient::at`, `ZebraIndexerWatch::at` → `new` (none probes the validator).
- `zaino-chainview`: `ObservationFold` → `ObservationMerge` (`ChainView::observation_merge()`), so "fold" names only block → index changes.

**Migration:** rename the items above.
