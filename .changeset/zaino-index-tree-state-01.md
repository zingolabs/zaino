---
default: minor
---

# `zaino-index-tree-state`: First release

First release. The commitment-tree index for Sapling, Orchard and Ironwood: retained tree nodes, per-height tree sizes and subtree roots (`schema(network)`), so any historical frontier rebuilds without hashing. `fold` / `fold_run` derive a block's (or a run's) changes with batched Merkle hashing across cores; `TreeStateReader` answers `treestate` and `subtree_roots` over any view, and a tree state writes a pool's tree only from its activation (`PoolActivations`). `TreeStateIndexWriter::new(store)` follows the final stream.
