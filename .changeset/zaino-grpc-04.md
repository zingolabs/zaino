---
default: major
---

# `zaino-grpc`: Every request reads one global snapshot (`Routes` reshaped)

Every request reads one global snapshot: `Routes { snapshots, submit, validators, network, max_address_rows }` (`snapshots` = `zaino_snapshot::Snapshots`, one `load()` per request or stream; `submit` = the chain view, `SendTransaction` only). `GetLatestBlock`, `GetBlockRange`, `GetTreeState`, `GetLightdInfo` and the mempool methods answer from the same snapshot. `GetMempoolStream` now ends when the **served** tip moves (after the NFS folds the new block), so the `GetLatestBlock` that follows serves it; before, it ended on the best block, possibly before the block was servable. `GetLightdInfo` reads no validator: the served height, the verified best as `estimatedHeight`, and the consensus branch and next upgrade from the upgrade schedule at the served height, all from one load (they can no longer disagree). It answers while no trusted validator vouches, and `UNAVAILABLE` until something is served (was `blockHeight: 0`). Tree-state memos are keyed per NFS publish.

**Migration:** `Routes { snapshots: publisher.handle(), submit: Arc::clone(&view), validators, network, max_address_rows }`; spawn the `zaino_snapshot::Publisher`.
