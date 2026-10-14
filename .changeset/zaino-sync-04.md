---
default: major
---

# `zaino-sync`: `check_block_at`, `fetch_at` and `Applied` no longer public; `PerIndex::at` replaces `at_mut`

`check_block_at`, `fetch_at` and the `Applied` alias are internal (`Run::blocks` = `Vec<(BlockHeight, Arc<T>)>`). `PerIndex::at_mut` replaced by the shared `PerIndex::at(position)`; `PerIndex::len` / `is_empty` added.
