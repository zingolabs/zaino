---
default: minor
---

# `zainod`: `[trusted_validators.indexer_tls]` for zakurad's indexer

Mutual TLS to a trusted validator's `indexer_address`, as zakurad's `[rpc.indexer_tls]` requires off loopback: `ca_path` verifies the validator, `cert_path` and `key_path` are zainod's client certificate (signed by the validator's `client_ca_file`), optional `server_name` (default the address's host). Both the push streams and `validator_client = "grpc"` use it. The files are read at boot; `indexer_tls` without `indexer_address` is refused.
