---
default: major
---

# `zainod`: accurate latencies, a state gauge, high-level per-index and peer metrics

- **gRPC:** `zaino_grpc_duration_seconds{method}` = one-reply RPCs' latency, `OK` answers only.
  Server streams moved out to `zaino_grpc_first_message_seconds{method}` (latency, now streams
  only).
- **Validators:** `zaino_validator_rpc_duration_seconds` = attempts the validator answered (an
  error reply included). No answer (refused connection, timeout) = `failures_total` only.
- **State:** `zaino_state{state}`, one-hot over `/readyz`'s six states.
- **Peers:** zebra-network's series cut to `zcash_net_peers`, `pool_num_{ready,unready}`,
  `zcash_net_{in,out}_messages{command}`, `zcash_net_{in,out}_bytes_total` and
  `zcash_net_connection_outcomes_total`, with no `addr`, `remote_ip`, `user_agent`,
  `remote_version`, `remote_services` or `error` label (counters summed over them). Every other `zcash_net_*`, `zebra_net_*`, `candidate_set_*`, `crawler_*`,
  `peer_*` and `pool_*` series is gone.
- **Storage:** `zaino_lsm_*` and `zaino_disk_{committed,cached}_bytes` lose `table` (one series
  per index, its tables summed); `zaino_lsm_*` also loses `tier`. `zaino_lsm_stall_at`,
  `zaino_lsm_batch_rows_total`, `zaino_lsm_merge_rows_total` and `zaino_lsm_merge_cancelled_total`
  are gone.
- **Removed:** `zaino_io_stall_seconds_total` (whole seconds: no signal; host PSI covers it).
