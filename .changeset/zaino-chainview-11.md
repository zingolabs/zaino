---
default: minor
---

# `zaino-chainview`: `ChainViewSnapshot::claimed_past_best()`: an answering validator claims a tip above best, not shown off our …

`ChainViewSnapshot::claimed_past_best()`: an answering validator claims a tip above best, not shown off our chain (its hash at best not polled yet counts). Feature `testing`: `ChainViewSnapshot::ahead(at, by, read)` (holder `at` claims `by` blocks past best; `read` = `Agreement::Ahead`, else `Diverged` and no longer a holder); `fixed` marks each holder `Agreed`, as a fold would.
