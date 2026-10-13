---
default: minor
---

# `zainod`: `snapshot` feature: `[snapshot]` (`manifest`, `connections`) fills empty indexes from a published snapshot …

`snapshot` feature: `[snapshot]` (`manifest`, `connections`) fills empty indexes from a published snapshot before boot, one archive per index (only the empty ones downloaded). The manifest lists each archive's `sha256`, `archive_bytes` and `installed_bytes`; free space is checked per filesystem before any download. aria2c (on `PATH`) queues every archive over `connections` in total and checks each sha256; each archive is unpacked beside its own index as it lands and renamed into place. `/statusz` `snapshot` reports `indexes` (archives fetched) and `total` (their bytes); a restart resumes each archive at the step it stopped in.
