---
default: major
---

# `zaino-chainview`: Feature `testing`: `testing::MockUtxos` is removed

A `MockChain` tip's `Ledger` implements `UnspentOutputs`.

**Migration:** Mine the funding outputs on the chain (e.g. a coinbase with a stated txid) and pass `Arc::new(chain.ledger(tip))`.
