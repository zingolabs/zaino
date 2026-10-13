---
default: minor
---

# `zaino-source`: `ChainDataSource::get_blocks_by_height(heights)`: `getblock "<height>" 0` per height in one JSON-RPC batch, …

`ChainDataSource::get_blocks_by_height(heights)`: `getblock "<height>" 0` per height in one JSON-RPC batch, decoded like `get_block_by_hash`, `heights` order; any height above the tip = `GetAtHeightError::HeightNotFound` for it. `MockValidator` answers it from its best chain, telling its scripted `Lie` too.
