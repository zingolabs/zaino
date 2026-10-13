---
default: patch
---

# `zaino-nfs`: `Nfs::run` returns `Ok` when an input's sender drops after the cancel (header sync or an index writer stops …

`Nfs::run` returns `Ok` when an input's sender drops after the cancel (header sync or an index writer stops on the same cancel), not `NfsError::ChainGone` / `IndexGone`.
