---
default: minor
---

# `zainod`: overload causes, network queueing, `zainod overload`

- `/readyz` `Overloaded: <cause>`: `CPU pressure`, `IO pressure` or `refusing work`,
  thresholds in `[grpc.overload]`.
- `zaino_grpc_queue_delay_seconds`: send queueing per client network every 10 s, against the
  network's 10 min base RTT, from sockets sending ≥ 100 segments in it (native histogram).
- `zaino_grpc_retransmit_ratio`: retransmitted share per client network every 10 s (native
  histogram). Both network histograms are observation only, never an overload cause.
- `zaino_state_reason{reason}`: `/readyz`'s detail, one-hot (`overloaded`, `runtime_stalled`,
  `no_validator_reachable`, `tip_unvouched`, `goaway_pending`, `goaway_sent`).
- `zaino_grpc_overload_latched{cause}`: each overload cause's latch, also while detection is off.
- `zainod overload on|off|set|unset`: overrides across restarts, as `zainod hold`.
- Library: `AdmissionReport::overload: Option<OverloadCause>` replaces `overloaded`;
  `GrpcLimits::overload`; `AdmissionState::set_overload`.
- Logs: `State changed` on each `/readyz` state or detail change, `Not ready` every 60 s while
  down (error), held or degraded (warn); `Overload latched` / `Overload cleared` per cause.
