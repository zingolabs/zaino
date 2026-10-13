---
default: major
---

# `zaino-grpc`: `Routes.version` and `lightd_info(snap, network, version)`: `GetLightdInfo.version` = the serving daemon's …

`Routes.version` and `lightd_info(snap, network, version)`: `GetLightdInfo.version` = the serving daemon's version, no longer this crate's own `CARGO_PKG_VERSION`.

**Migration:** Pass the binary's version: `Routes { version: env!("CARGO_PKG_VERSION"), .. }`, `lightd_info(snap, network, env!("CARGO_PKG_VERSION"))`.
