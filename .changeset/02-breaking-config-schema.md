---
default: major
---

# `zainod`: new config schema; unknown keys are refused

The config file is replaced. A key zainod does not know fails the load, so a 0.10 file does not
start 0.11. Start from `zainod generate-config` (every key at its default) or
`docs/example_configs/zainod.toml`.

Top level: `network`, `validator_client`, `[metrics]`, `[[trusted_validators]]`, `[serve]`
(+ `[serve.tls]`), `[grpc]`, `[submission]`, `[p2p]`, `[sync]` (+ `[sync.lsm_store]`),
`[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]`,
`[index.transparent_address]` and `[snapshot]`.

| 0.10 key | 0.11 key |
| --- | --- |
| `network = "Mainnet" \| "Testnet" \| "PubTestnet" \| "Regtest"` | `network = "mainnet" \| "testnet" \| "regtest"` (default `mainnet`) |
| `[validator_settings]` | one `[[trusted_validators]]` entry (add more for redundancy) |
| `validator_jsonrpc_listen_address` | `jsonrpc_address` (default `127.0.0.1:8232`) |
| `validator_cookie_path` | `cookie_path` |
| `validator_user` / `validator_password` | `user` / `password` |
| `validator_grpc_listen_address` | none (`indexer_address` names zebrad's indexer gRPC) |
| `grpc_settings.listen_address` | `serve.grpc_listen_address` |
| `[grpc_settings.tls]` `cert_path`, `key_path` | `[serve.tls]` `cert_path`, `key_path` |
| `metrics_endpoint` | `metrics.listen_address` |
| `ZAINO_*` environment overrides | `ZAINO_CONFIG_*` (`__` nests: `ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS`) |

Removed with no replacement: `backend`, `zebra_db_path`, `ephemeral_finalised_state`,
`json_server_settings`, `service`, `storage` (`cache`, `database`), `mempool` and
`donation_address`.

Behaviour of the moved keys:

- `network` is declared, never read off the validator (Zebra on regtest reports `test`).
- `user` and `password` are set together or not at all. With neither, no basic auth is sent
  (0.10 sent `xxxxxx:xxxxxx` by default).
- Checked at startup: at least one and at most 64 `[[trusted_validators]]`, no address listed
  twice, at least one index enabled, and `sync.finalised_depth` ≥ 1000 off regtest.

New keys are described in the entries for the features they configure.
