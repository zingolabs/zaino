---
default: minor
---

# `zaino-source`: what a validator reports about itself

New (from `zaino-primitives`): `BlockchainInfo` (only what Zaino reads: `blocks`, `estimated_height`, `best_block_hash`, `sapling_activation`, `upgrades`, `consensus`; `schedule()` = its listing as a `LocalNetwork`), `NetworkUpgradeInfo` / `NetworkUpgradeStatus` / `ConsensusBranchIds` (branch ids are `zcash_protocol`'s `BranchId`), `NodeRelease` and `EndOfService`, `PeerInfo`. Feature `testing`: `ValidatorViews::blockchain_info(tip)`.
