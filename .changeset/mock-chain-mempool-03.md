---
default: major
---

# `zaino-source`: Feature `testing`: a `MockValidator` comes from `chain.validator(tip)` and lists what it hears from the chain's mempool

`MockValidator::following`, `mempool_insert` and `mempool_remove` are removed. The `ValidatorViews` trait on `MockChain` gives `validator(tip)`. A validator's mempool holds entries of the chain's it `hear`s plus the sends it accepts, each admitted on its own tip by the chain's `Ledger` at the fee its bytes leave; `follow` re-admits it on the new tip. A send refused there answers `Rejected { code: -25 }` naming the `Refused` rule.

**Migration:** `MockValidator::following(&chain, tip)` → `chain.validator(tip)`; `mempool_insert(raw, fee)` → `let txid = chain.submit(decoded(raw)); validator.hear(&chain, txid)`, funding any inputs on the chain (a stated fee is now the derived one); `mempool_remove(txid)` → `evict(txid)`.
