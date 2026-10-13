---
default: major
---

# `zaino-index-compact-block`: The `testing` feature (`testing::{block, chain, committed}`) is removed

The `testing` feature (`testing::{block, chain, committed}`) is removed.

**Migration:** Mine blocks with `zaino_primitives::testing::MockChain` and fold them with `fold` + `chain.fees(hash)`.
