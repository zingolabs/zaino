---
default: major
---

# `zaino-internal-block-hash-to-height`: `fold(parent: &BlockHashReader<V>, block, out: &mut BlockChanges)` appends to a caller-opened delta …

`fold(parent: &BlockHashReader<V>, block, out: &mut BlockChanges)` appends to a caller-opened delta (asserting it is the block's and the block extends the parent), replacing `fold(block, network) -> BlockChanges`; it moved into `writer.rs`. `schema(network)` is replaced by `FORMAT` + `TABLES`.

**Migration:** Fold onto a `BlockHashReader` of the parent into `store.changes(block.at())` (or a layer's); open the store with `Schema::new(IndexKind::BlockHash, FORMAT, network, TABLES)`.
