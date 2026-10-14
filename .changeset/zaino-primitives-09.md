---
default: major
---

# `zaino-primitives`: validator reports move to `zaino-source`; aliases removed

`BlockchainInfo` moves to `zaino-source`, beside the parser that builds it (`MockChain::blockchain_info` = `zaino_source::testing::ValidatorViews::blockchain_info`); `NetworkUpgradeInfo`, `NetworkUpgradeStatus` and `ConsensusBranchIds` are removed (nothing read them). `BlockTime`, `EquihashNonce` and `OutputIndex` are removed: those fields are `u32` / `[u8; 32]`.
