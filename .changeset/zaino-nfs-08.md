---
default: major
---

# `zaino-nfs`, `zaino-index-tree-state`: "above durable" names for the NFS window

The NFS holds every block above the indexes' durable tips, finalized blocks not yet durable included, so names that said "non-finalized" for that window now say what they mean: `Indexed::served_nonfinalized` → `served_above_durable`, `TreeStateReader::is_non_finalized` → `is_above_durable`.

**Migration:** rename the two calls.
