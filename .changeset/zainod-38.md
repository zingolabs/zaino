---
default: major
---

# `zainod`: `[grpc]` stream and timeout keys renamed for what they bound

`[grpc] stall_timeout_secs` splits into `unread_timeout_secs` (data a client leaves unread → connection closed) and `silent_timeout_secs` (nothing to send → stream `UNAVAILABLE`), both 300 by default. `max_streams` → `max_work_streams`, `adaptive_streams` → `adaptive_work_streams`. A config naming an old key is refused as an unknown field. `/statusz` `load.refused_per_s` = the last minute's work refusals per second (was a ~10 s decay).

**Migration:** rename the keys; set both `unread_timeout_secs` and `silent_timeout_secs` where `stall_timeout_secs` was set.
