---
default: minor
---

# `zaino-internal-block-hash-to-height`: First release

First release. The block hash → height index every by-hash request resolves through: one map `by_hash` (`schema(network)`), one row per block (`fold`), read with `BlockHashReader::height_of` over any view. `BlockHashIndexWriter::new(store)` follows the final stream.
