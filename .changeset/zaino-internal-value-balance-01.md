---
default: minor
---

# `zaino-internal-value-balance`: First release

First release. The value-balance index: every transparent output's value in one insert-only map `outputs` (`OutPoint` → zatoshis, `schema(network)`). `fold(parent, block)` resolves each transaction's `Fee` (transparent in − out plus the Sprout, Sapling, Orchard and Ironwood value balances) with one batched prevout lookup per run, read through `ValueBalanceReader`; a negative fee on a non-coinbase transaction is consensus-invalid (`FoldError`). `ValueBalanceIndexWriter::new(store)` follows the final stream and publishes `BlockFees` into a `zaino_sync::FeeSink` for the compact-block index (one per step, held heights re-priced).
