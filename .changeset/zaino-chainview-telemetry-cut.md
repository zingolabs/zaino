---
default: major
---

# `zainod`: chain view telemetry cut to what an operator acts on

- **Removed:** `zaino_chainview_first_trusted_seconds`, `zaino_chainview_all_trusted_seconds`,
  `zaino_chainview_residence_seconds` and `zaino_chainview_shared_outbound_min`.
- **Alarms:** `partitioned` and `eclipsed` are gone (their WARN / INFO edge logs and `/status`
  notes with them). `stale`, `end of service` and `finality paused` remain.
