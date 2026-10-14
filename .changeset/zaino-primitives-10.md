---
default: major
---

# `zaino-primitives`: `BlockHash` is `zcash_protocol::BlockHash`

`types::BlockHash` and `ParseHashError` are removed: every API naming a block hash takes `zcash_protocol::BlockHash` (bytes via `.0` / `BlockHash(bytes)`, display hex via `from_hex` / `as_hex`). `Target`'s `FromStr` is now `Target::from_hex`.
