---
default: major
---

# `zaino-persistence`: `PersistenceEngine::open(path, schema, write_buffer: NonZeroUsize)`: the store commits by itself in …

`PersistenceEngine::open(path, schema, write_buffer: NonZeroUsize)`: the store commits by itself in `Store::apply` once its buffer (`buffered_bytes`, heap) reaches `write_buffer`; that commit failing panics through `StoreError::commit_failed` (index + directory named). `conformance::contract` checks it.

**Migration:** Pass the index's `WRITE_BUFFER` to `open` and drop any `buffered_bytes()` threshold check before `commit`; call `commit` only to make a buffer durable early (the final tip, shutdown). `NonZeroUsize::MAX` = commits only when told.
