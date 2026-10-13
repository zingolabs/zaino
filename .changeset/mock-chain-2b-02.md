---
default: major
---

# `zaino-source`: Feature `testing`: `mock::MockChain`, `mock::MEMPOOL_FEE` and `mock::fixture_transactions` are removed

Feature `testing`: `mock::MockChain`, `mock::MEMPOOL_FEE` and `mock::fixture_transactions` are removed.

**Migration:** Serve a `zaino_primitives::testing::MockChain` tip with `testing::MockValidator::following`; reorg with `follow`; read fixtures from `testing::fixtures::transactions`.
