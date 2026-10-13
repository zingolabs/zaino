---
default: patch
---

# `zaino-sync`: `FinalFollower::run` returns `Ok` when the verified chain's sender drops after the cancel (header sync stops …

`FinalFollower::run` returns `Ok` when the verified chain's sender drops after the cancel (header sync stops on the same cancel), not `FollowError::ChainGone`.
