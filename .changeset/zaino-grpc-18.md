---
default: major
---

# `zaino-grpc`: `GrpcLimits` names each stream and timeout for what it bounds; `Load` → `AdmissionState`

- `stall_timeout` splits in two: `unread_timeout` (data the client leaves unread → connection closed) and `silent_timeout` (nothing to send → stream `UNAVAILABLE`, `GetMempoolStream` exempt). Both default to 300 s.
- `max_streams` → `max_work_streams` and `adaptive_streams` → `adaptive_work_streams` (admitted work only; `max_streams_per_connection` keeps counting every h2 stream). The refusal message reads `work stream limit reached`.
- `Load` / `LoadReport` / `GrpcService::load()` → `AdmissionState` / `AdmissionReport` / `GrpcService::admission()`. `refused_per_s` and `overloaded` now move once a minute, at the serving summary (`refused_per_s` = the last minute's work refusals per second; `overloaded` judged per whole minute, never on read).
- The adaptive work limit is exact: admission never holds more work streams than the limit.
- `GetLatestBlock` no longer needs the compact-block index (it answers the served tip).
- A `GetBlockRange` cut short by shutdown ends with its `UNAVAILABLE` trailers instead of an `OK`-looking end.
- The serving log's `High load` field `streams=` is `work=`.

**Migration:** rename the `GrpcLimits` fields and `Load` types as above; set `unread_timeout` and `silent_timeout` where `stall_timeout` was set.
