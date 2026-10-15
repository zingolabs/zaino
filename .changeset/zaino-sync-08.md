---
default: patch
---

# `zaino-sync`: A by-height batch stops where the verified headers begin

`FinalizedFollower` batched heights by height up to its lookahead even past the first height the verified chain holds a header for, so blocks it could check against a header came unchecked from one trusted validator. A batch now stops at the first held header; from there each block is fetched by hash and checked, as documented.
