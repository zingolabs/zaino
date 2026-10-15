---
default: major
---

# Releases: one workspace version, the `zainod` image only, nothing on crates.io

- Every crate ships at one version, released as tag `vX.Y.Z` (was per-crate `zainod-X.Y.Z`), with
  one `CHANGELOG.md`.
- Docker Hub: `zainod:X.Y.Z` and `zainod:latest` only. The `zaino` repository and the `-no-tls`
  image variant are no longer published.
- No crate is published to crates.io.
- `zainod` has no cargo features: `prometheus`, `no_tls_use_unencrypted_traffic`,
  `no_tls_with_prometheus`, `allow_unencrypted_public_json_rpc_bind`, `experimental_features`,
  `transparent_address_history_experimental`, `test_dependencies` and `fast-test-seam` are gone.
  Metrics, the admin listener, the transparent-address index and the `[snapshot]` bootstrap are
  always built. Building needs `protoc` (on `PATH` or named by `PROTOC`).

Container image:

- Entrypoint `zainod`, default command `start`; `entrypoint.sh` is gone. Configure through
  `/app/config/zainod.toml` or `ZAINO_CONFIG_*` variables. `ZAINO_STORAGE__DATABASE__PATH` and
  `ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR` are no longer read.
- Indexes live under `/app/data/indexes`. The hold marker's `[serve] state_dir` defaults outside
  `/app`: point it at a mounted path for a hold to outlive the container.
- Exposes only the gRPC port, 8137 (no JSON-RPC port).
- No `HEALTHCHECK`: probe `/livez` and `/readyz` on the admin listener, or `/readyz` on the gRPC
  port.
- Ships `aria2c` for the `[snapshot]` bootstrap. The `CARGO_FEATURES` build argument is gone.
