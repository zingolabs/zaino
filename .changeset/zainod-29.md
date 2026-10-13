---
default: minor
---

# `zainod`: `zainod hold` / `zainod resume [--config FILE]`: an operator hold that survives restarts, deploys and reboots

`zainod hold` / `zainod resume [--config FILE]`: an operator hold that survives restarts, deploys and reboots. The marker lives in the new `[serve] state_dir` (default `<data dir>/zaino/state`); the running zainod reads it every second. Held = state `held` on `/statusz`, `/readyz` (503) and `/status`, above every other state (a held node stopping still reports `held`); after `[serve] hold_goaway_delay_secs` (default 0) every open gRPC connection is sent GOAWAY, while health and admin keep answering, and `zainod hold` returns only once a running zainod sent them (hold = the drain before a planned stop). A stopped zainod is never reported held. Under systemd a node started held completes its start. Both commands are idempotent, print the resulting state and exit 0 (1 on a config or state-dir error).
