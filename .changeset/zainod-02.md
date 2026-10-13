---
default: major
---

# `zainod`: The config schema is replaced and unknown keys are refused at load

The config schema is replaced and unknown keys are refused at load. Top-level tables: `network`, `[metrics]`, `[[trusted_validators]]`, `[serve]` (+ `[serve.tls]`), `[grpc]`, `[submission]`, `[p2p]`, `[sync]`, `[index.*]` and `[snapshot]`. Environment overrides take the `ZAINO_CONFIG_` prefix (`__` nesting) instead of `ZAINO_`. `network` is `mainnet`, `testnet` or `regtest` (default `mainnet`), declared and never read off the validator.

**Migration:** Start from `zainod generate-config` or `docs/example_configs/zainod.toml`. Map: `validator_settings` → one `[[trusted_validators]]` entry (`validator_jsonrpc_listen_address` → `jsonrpc_address`, `validator_cookie_path` → `cookie_path`, `validator_user` / `validator_password` → `user` / `password`); `grpc_settings.listen_address` → `serve.grpc_listen_address`; `grpc_settings.tls` → `[serve.tls]` (same `cert_path`, `key_path`); `metrics_endpoint` → `metrics.listen_address`; `network = "Testnet"` → `"testnet"` (likewise `mainnet`, `regtest`). Delete `backend`, `zebra_db_path`, `ephemeral_finalised_state`, `json_server_settings`, `service`, `storage`, `mempool`, `donation_address` and `validator_grpc_listen_address`. Rename `ZAINO_*` environment variables to `ZAINO_CONFIG_*`.
