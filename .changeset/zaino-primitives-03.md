---
default: minor
---

# `zaino-primitives`: New: `Fee` and `BlockFees`; `NodeRelease` and `EndOfService`; `PeerInfo`; `ReorgDepth`; `PerPool<T>` / …

New: `Fee` and `BlockFees`; `NodeRelease` and `EndOfService`; `PeerInfo`; `ReorgDepth`; `PerPool<T>` / `TreeSizes`; `OutPoint::{encode, decode}` (`txid ‖ vout`, big-endian, byte order = `Ord`); `MerkleRoot::of_txids` (zcashd's merkle root, refusing a repeated pair, CVE-2012-2459); `Block::footprint()`; `Height::{next, up_to}`; `TreeSize::checked_add`; `Script::as_bytes`; `network::{chain_name, network_name}`; `protocol::{MAX_BLOCK_REORG_HEIGHT, MAX_BLOCK_BYTES}`. Feature `testing`: `testing::Chain`, a regtest block-tree builder with real header bytes, plus `encode_header`, `header_hash` and `linked`.
