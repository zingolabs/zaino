---
default: minor
---

# `zainod`: `zainod hold` / `zainod resume` take a node out of rotation

- `zainod hold` writes a marker in `[serve] state_dir` (default `<data dir>/zaino/state`) that
  survives restarts, deploys and reboots. The running zainod reads it within a second: state
  `held` (`/readyz` `503`), and after `[serve] hold_goaway_delay_secs` (default 0) every gRPC
  connection, open or opened later, is sent GOAWAY. Health and admin requests keep answering.
- `zainod hold` returns once the running zainod reports `GOAWAY sent`, then prints `safe to stop`.
  `zainod resume` lifts the hold. Both are idempotent and exit 0 (1 on a config or state-directory
  error).
- SIGTERM / SIGINT closes the gRPC listener at once and gives open connections
  `[grpc] shutdown_grace_secs`, then exits 0. Behind a load balancer, hold before stopping.
- Under systemd, a node started held completes its start.
