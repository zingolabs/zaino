---
default: minor
---

# `zaino-primitives`: `types::UpgradeSchedule`: one table of network-upgrade activation heights

`types::UpgradeSchedule`: one table of network-upgrade activation heights. `compiled(network)` = mainnet's or testnet's (`zcash_protocol`'s tables, `None` for regtest); `reported(&info)` = a validator's `getblockchaininfo` (regtest). The `testing` feature's `Upgrades` builder is now `UpgradeSchedule`'s test-only methods (`all_at`, `with`, `onward`, `without`).
