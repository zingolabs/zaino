---
default: major
---

# `zaino-primitives`: validator reports move to `zaino-source`; aliases removed

`BlockchainInfo`, `NetworkUpgradeInfo`, `NetworkUpgradeStatus` and `ConsensusBranchIds` move to `zaino-source`, beside the parser that builds them (`MockChain::blockchain_info` = `zaino_source::testing::ValidatorViews::blockchain_info`). `BlockTime`, `EquihashNonce` and `OutputIndex` are removed: those fields are `u32` / `[u8; 32]`.
