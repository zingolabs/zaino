---
default: major
---

# `zaino-persistence`: `LsmConfig::merge_mib_per_sec` (default 200): every merge of one engine shares that many read + written MiB/s …

`LsmConfig::merge_mib_per_sec` (default 200): every merge of one engine shares that many read + written MiB/s (one second of burst), so merges never take the whole device from commits' fsyncs and folds' lookups. Commits are not paced.

**Migration:** Add `merge_mib_per_sec` where an `LsmConfig` is built field by field (or use `..LsmConfig::default()`).
