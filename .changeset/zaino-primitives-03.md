---
default: minor
---

# `zaino-primitives`: New: `Fee` and `BlockFees`; `ReorgDepth`; `PerPool<T>` / `TreeSizes`; …

New: `Fee` and `BlockFees`; `ReorgDepth`; `PerPool<T>` / `TreeSizes`; `MerkleRoot::of_txids` (zcashd's merkle root, refusing a repeated pair, CVE-2012-2459); `Block::footprint()`; `TreeSize::checked_add`; `protocol::{MAX_BLOCK_REORG_HEIGHT, MAX_BLOCK_BYTES}`. Feature `testing`: `testing::Chain`, a regtest block-tree builder with real header bytes, plus `encode_header`, `header_hash` and `linked`.
