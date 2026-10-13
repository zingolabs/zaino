---
default: major
---

# `zaino-index-tree-state`: `fold(parent, block, out: &mut BlockChanges) -> Result<(), FoldError>` appends to a caller-opened delta …

`fold(parent, block, out: &mut BlockChanges) -> Result<(), FoldError>` appends to a caller-opened delta (asserting it is the block's and the block extends the parent); it and the run fold moved into `writer.rs`, the run fold filling one caller-opened delta per block (same batched hashing). `schema(network)` is replaced by `FORMAT` + `TABLES` (the 100 tables as constants); `TreeStateReader::new(view)` takes no network.

**Migration:** Open the delta with `store.changes(block.at())` (or a layer's), open the store with `Schema::new(IndexKind::TreeState, FORMAT, network, TABLES)`, drop the reader's network.
