---
default: major
---

# `zaino-chainview`: The view no longer serves on its own: `ChainTip`, `Unserved`, `ChainViewSubscriber::{tail, subscribe_tip}`, …

The view no longer serves on its own: `ChainTip`, `Unserved`, `ChainViewSubscriber::{tail, subscribe_tip}`, `ChainViewSnapshot::{tip, unserved}`, `MempoolTail` / `Logged` (the feed) and the per-fold `zaino.chainview.*` gauges and alarm logs are gone. `zaino-snapshot` keeps the `GetMempoolStream` feed (keyed by the served tip), sets the gauges at scrape and logs the alarm edges per publish. `ChainViewSnapshot::held_by()` names the trusted validators holding the best block; `mempool()` and `validator_info()` return `Option` (`None` = no holder; the reason is the global snapshot's `Unavailable`); `shared_outbound_min()` and `EndpointSet::contains` are public. Publishes are stored in fold order.

**Migration:** Read the mempool, `GetLightdInfo` facts and the stream through `zaino_snapshot::Snapshot` (`mempool()?`, `lightd()?`, `mempool_stream()?`); `best()` + `held_by()` replace `tip()`.
