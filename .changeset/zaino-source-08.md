---
default: major
---

# `zaino-source`: `getblockchaininfo` refuses an unknown branch id or an out-of-order schedule

`getblockchaininfo` fails to parse when an upgrade or the consensus tip/next block carries a branch id `zcash_protocol` does not know, or when the listed activation heights run out of protocol order. `Prepared::branch` is a `zcash_protocol::consensus::BranchId`.
