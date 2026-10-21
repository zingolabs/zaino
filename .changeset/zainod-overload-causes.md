---
default: minor
---

# `zainod`: overload causes, network queueing, `zainod overload`

- `/readyz` `Overloaded: <cause>`: `network saturated`, `CPU pressure`, `IO pressure` or
  `refusing work`, thresholds in `[grpc.overload]` (network off by default).
- `zaino_grpc_queue_delay_seconds`: send queueing per client network every 10 s (native
  histogram).
- `zainod overload on|off|set|unset`: overrides across restarts, as `zainod hold`.
- Library: `AdmissionReport::overload: Option<OverloadCause>` replaces `overloaded`;
  `GrpcLimits::overload`; `AdmissionState::set_overload`.
