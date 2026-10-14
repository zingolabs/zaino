---
default: major
---

# `zaino-primitives`: network-upgrade schedules and branch ids are `zcash_protocol`'s

Activation heights are a `zcash_protocol::local_consensus::LocalNetwork`: `protocol::compiled_schedule(network)` = mainnet's or testnet's (`None` for regtest). `ConsensusBranchId` is removed for `zcash_protocol::consensus::BranchId`. The `testing` feature's `all_at(height)` builds a regtest schedule (edit it with struct-update syntax).
