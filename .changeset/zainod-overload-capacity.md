---
default: minor
---

# `zainod`: `Overloaded` when the node lacks capacity, whatever it lacks

`/readyz` turns `down` with `detail` `Overloaded: <cause>` (`egress queue`, `retransmits`, `CPU
pressure`, `IO pressure`, `refusing work`), so a load balancer sends new wallets elsewhere. Each
cause is measured, with no link speed or machine size configured: the listener's own sockets'
queueing delay (RTT over min RTT) and retransmits across ≥ 20 client networks, and zainod's cgroup
CPU / IO pressure (Linux PSI). A cause latches after 1–2 minutes over its threshold and clears
after 3 minutes under half of it.

- Config: `[grpc.overload]` `egress_queue_delay_ms` (10), `egress_min_networks` (20),
  `retransmit_percent` (5), `cpu_pressure_percent` (25), `io_pressure_percent` (25); a percent of 0
  turns that cause off.
- Incidents: `zainod overload off|on` (never report `Overloaded` / again) and `zainod overload
  set|unset <param> [value]` (one threshold over the config's), as markers in `[serve] state_dir`
  that persist like `zainod hold`'s; the running zainod applies them within a second and names
  them in `/status` notes.
- Metrics: `zaino_grpc_egress_queue_delay_seconds`, `zaino_grpc_egress_retransmit_ratio`,
  `zaino_grpc_egress_networks`, `zaino_grpc_cpu_pressure_ratio`, `zaino_grpc_io_pressure_ratio`,
  `zaino_grpc_overloaded{cause}`, `zaino_grpc_overload_detection`.
- Library: `zaino_grpc::AdmissionReport::overload: Option<OverloadCause>` replaces `overloaded`;
  `GrpcLimits::overload: OverloadLimits`; `AdmissionState::set_overload` applies an operator's
  thresholds live.
