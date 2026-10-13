---
default: major
---

# `zaino-index-transparent-address`: `fold(parent, block, out: &mut BlockChanges)` appends to a caller-opened delta (asserting it is the block's …

`fold(parent, block, out: &mut BlockChanges)` appends to a caller-opened delta (asserting it is the block's and the block extends the parent); it moved into `writer.rs`. `schema(network)` is replaced by `FORMAT` + `TABLES`; `TransparentAddressReader::new(view)` takes no network.

**Migration:** Open the delta with `store.changes(block.at())` (or a layer's), open the store with `Schema::new(IndexKind::TransparentAddress, FORMAT, network, TABLES)`, drop the reader's network.
