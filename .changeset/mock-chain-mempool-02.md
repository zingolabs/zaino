---
default: major
---

# `zaino-primitives`: Feature `testing`: `testing::fee_left` is removed

**Migration:** Check a transaction with `MockChain::ledger(tip).admit(&tx)`, which returns its fee or the `Refused` rule.
