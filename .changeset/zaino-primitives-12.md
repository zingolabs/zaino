---
default: major
---

# `zaino-primitives`: `TreeSizes::advance` panics past 2^32 leaves

`TreeSizes::advance(&block)` returns the sizes directly and panics on a commitment tree past 2^32 leaves (a depth-32 tree holds no more; consensus never builds one). `zaino_index_compact_block::fold` returns `()` and the NFS's `FoldError::CompactBlock` is gone.
