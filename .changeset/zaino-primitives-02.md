---
default: major
---

# `zaino-primitives`: Reshaped types

Reshaped types. `Block`'s fields are private: `Block::new(header, transactions)` (asserts a coinbase), `header()`, `transactions()`, and it no longer carries `ChainMetadata`, which `PerPool<T>` / `TreeSizes` replace. `TransparentData` holds `coinbase` and `inputs: Vec<OutPoint>` (`TransparentInput` removed) and `Transaction` gains `sprout: SproutData`. `Treestate`'s trees are `CommitmentTreeBytes` (replacing `PoolTreestate` and `TreeBytes`). `SubtreeRoot.completing` (a `BlockRef`) replaces `end_height`.

**Migration:** Build blocks with `Block::new` and read them through `header()` / `transactions()`; read tree sizes from `TreeSizes`; match `SubtreeRoot.completing.height` where you read `end_height`.
