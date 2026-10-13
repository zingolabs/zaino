---
default: major
---

# `zaino-persistence`: `Store::buffered_bytes` reports the heap the buffer holds, not its item bytes: per item its `Bytes` handle, …

`Store::buffered_bytes` reports the heap the buffer holds, not its item bytes: per item its `Bytes` handle, allocation and tree slot, per block its delta, derived from the buffer's types and within ±30% of the measured heap for fixed, variable and map tables (`tests/buffer_heap.rs`, counting allocator). Item bytes undercounted the buffer's RAM 2–10×, so a 64 MiB batch held up to ~750 MB before its first commit.

**Migration:** A batch budget compared against `buffered_bytes()` is now RAM: the same figure commits 2–10× sooner. Size it as the buffer's heap, not bytes on disk.
