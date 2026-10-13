---
default: major
---

# `zaino-internal-value-balance`: `fold(parent, block, out: &mut BlockChanges) -> Result<BlockFees, FoldError>` appends the block's output rows …

`fold(parent, block, out: &mut BlockChanges) -> Result<BlockFees, FoldError>` appends the block's output rows to a caller-opened delta (asserting it is the block's and the block extends the parent) and returns its fees; it moved into `writer.rs`. New `fees(parent, blocks)`: a run's fees alone, no rows, against any state at or past its parent (held heights). `schema(network)` is replaced by `FORMAT` + `TABLES`; `ValueBalanceReader::new(view)` takes no network; `ValueBalanceIndexWriter::run`'s fee sink parameter is named `sink`.

**Migration:** Open the delta with `store.changes(block.at())` (or a layer's) and keep the returned fees; price a block the parent already holds with `fees`; open the store with `Schema::new(IndexKind::ValueBalance, FORMAT, network, TABLES)`.
