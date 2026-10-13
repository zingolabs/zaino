---
default: major
---

# `zaino-primitives`: Feature `testing`: `testing::Chain` (`mine_with`, `mine_at`, `mine_bits`, `mine_heavier`, `extend`, `path`) …

Feature `testing`: `testing::Chain` (`mine_with`, `mine_at`, `mine_bits`, `mine_heavier`, `extend`, `path`) and `testing::linked` are removed; `encode_header` and `header_hash` stay.

**Migration:** Build blocks with `testing::MockChain` (`mine`, `mine_empty`, `branch` / `fork`, `outweigh`, `blocks`).
