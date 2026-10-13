---
default: major
---

# `zaino-traffic`: Scheduling simplified: the request-rate GCRA, the retry budget, p95 hedge delays and PeakEWMA / …

Scheduling simplified: the request-rate GCRA, the retry budget, p95 hedge delays and PeakEWMA / power-of-two-choices are deleted; seven per-class reserves and ceilings become three lanes (control, interactive, bulk) with one reserved permit each, so header runs get the control reserve instead of waiting behind bulk; the poll keeps a permit of its own outside the lanes. `Limits::MIN_CONNECTIONS` is 5 (was 6); `Limits::new` takes only `max_connections`. `MemberRow.latency` is the last reply's round trip.

**Migration:** `Limits::new(n, rate)` → `Limits::new(n)`.
