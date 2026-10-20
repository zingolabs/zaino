---
default: major
---

# `zainod`: accurate latencies, a state gauge, no peer identity in metrics

- **gRPC:** `zaino_grpc_duration_seconds{method}` = one-reply RPCs' latency, `OK` answers only.
  Server streams moved out: `zaino_grpc_stream_duration_seconds{method}` (lifetime, new) and
  `zaino_grpc_first_message_seconds{method}` (latency, now streams only);
  `zaino_grpc_stream_messages` streams only.
- **Validators:** `zaino_validator_rpc_duration_seconds` = attempts the validator answered (an
  error reply included). No answer (refused connection, timeout) = `failures_total` only.
- **State:** `zaino_state{state}`, one-hot over `/readyz`'s six states.
- **Peers:** zebra-network's `zcash_net_*` / `zebra_net_*` lose `addr`, `remote_ip`,
  `user_agent`, `remote_version`, `remote_services` and `error` (counters summed over them);
  `zcash_net_peers_version_{connected,obsolete}` and `zebra_net_connection_state` are gone.
- **Removed:** `zaino_io_stall_seconds_total` (whole seconds: no signal; host PSI covers it).
