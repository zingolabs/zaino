---
default: major
---

# `zaino-header-chain`: Feature `testing`: `HeaderChain::{regtest_in_memory, insert_blocks}` and `VerifiedChain::regtest` are …

Feature `testing`: `HeaderChain::{regtest_in_memory, insert_blocks}` and `VerifiedChain::regtest` are removed; `Params::with_genesis` is crate-private. New: `testing::HeaderViews::header_params()`.

**Migration:** Use `testing::HeaderViews` on a `MockChain` (`header_chain`, `verified`, `verified_final`, `header_params`) and `testing::insert`.
