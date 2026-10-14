---
default: patch
---

# `zaino-index-tree-state`: a subtree's completing height is stored big-endian

`<pool>/subtrees` entries store `end_height` big-endian (a locator, as every height in Zaino's indexes); the record's other fields keep their protocol encoding.
