---
default: major
---

# `zaino-index-compact-block`: `fold(parent, block, fees, out: &mut BlockChanges) -> Result<(), TreeSizeOutOfRange>` appends to a …

`fold(parent, block, fees, out: &mut BlockChanges) -> Result<(), TreeSizeOutOfRange>` appends to a caller-opened delta (asserting it is the block's and the block extends the parent); it moved into `writer.rs`. `schema(network)` is replaced by `FORMAT` + `TABLES`; `CompactBlockReader::new(view)` takes no network.

**Migration:** Open the delta with `store.changes(block.at())` (or a layer's), open the store with `Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES)`, drop the reader's network.
