---
default: major
---

# `zainod`: Cargo features `prometheus`, `no_tls_use_unencrypted_traffic`, `no_tls_with_prometheus`, …

Cargo features `prometheus`, `no_tls_use_unencrypted_traffic`, `no_tls_with_prometheus`, `allow_unencrypted_public_json_rpc_bind`, `experimental_features`, `transparent_address_history_experimental`, `test_dependencies` and `fast-test-seam` are removed: metrics and the admin listener are always compiled in, and the transparent-address methods are always available. The one feature is `snapshot`.

**Migration:** Drop these from `--features` and from the Docker `CARGO_FEATURES` build argument.
