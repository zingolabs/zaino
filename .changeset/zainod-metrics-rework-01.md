---
default: minor
---

# `zainod`: The metrics listener (`metrics_endpoint`) runs on its own thread and runtime and also serves `/livez` (fails …

The metrics listener (`metrics_endpoint`) runs on its own thread and runtime and also serves `/livez` (fails once the indexer loop stops reporting for 30s).
