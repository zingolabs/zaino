---
default: minor
---

# `zaino-primitives`: `Block::check_body()`: its transactions are the ones its header commits to (`BodyMismatch::MerkleRoot`, or …

`Block::check_body()`: its transactions are the ones its header commits to (`BodyMismatch::MerkleRoot`, or `Mutated` for a repeated txid pair, CVE-2012-2459). `UpgradeSchedule::{active, branch_at, next_after}`: the upgrade in force and the next scheduled one at a height.
