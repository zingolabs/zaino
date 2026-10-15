---
default: major
---

# `zaino-source`: `ZebraIndexerTls` fields `ca_path` / `cert_path` / `key_path`, deserializable

`ZebraIndexerTls { ca, cert, key, server_name }` is now `{ ca_path, cert_path, key_path, server_name }` and derives `Deserialize` / `Serialize` (unknown keys refused), so zainod's `[trusted_validators.indexer_tls]` table parses straight into it.
