---
default: major
---

# `zaino-internal-value-balance`: `outputs` holds unspent outputs only: `fold` removes each prevout a block spends, and an output created and …

`outputs` holds unspent outputs only: `fold` removes each prevout a block spends, and an output created and spent in one block writes neither row. A new `fees` sequence stores each block's fees, written by `fold`; `ValueBalanceReader::block_fees(block)` reads a held block's back (`FoldError::StoredFeesMissing` / `StoredFeesUnreadable`), and the writer republishes held heights from it. `fees(parent, blocks)` is removed (a held block's prevouts may since be spent). Prevouts resolve transaction by transaction: a second spend of one output is `MissingPrevout`. `ValueBalanceReader<V>` and the writer's view need `SequenceRead + MapRead`.

**Migration:** Read a held block's fees with `ValueBalanceReader::block_fees(&block)` instead of `fees(&parent, &[&block])`.
