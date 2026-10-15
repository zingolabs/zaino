---
default: minor
---

# `zaino-grpc`: `TlsFiles` deserializable

`TlsFiles { cert_path, key_path }` derives `Deserialize` / `Serialize` (unknown keys refused), so zainod's `[serve.tls]` table parses straight into it.
