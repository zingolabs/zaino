---
default: major
---

# `zainod`: The container image drops `entrypoint.sh`: its entrypoint is `zainod` (default argument `start`), reading …

The container image drops `entrypoint.sh`: its entrypoint is `zainod` (default argument `start`), reading `/app/config/zainod.toml`, and it exposes only the gRPC port (8137).

**Migration:** Pass configuration through the mounted `zainod.toml` or `ZAINO_CONFIG_*` variables; `ZAINO_STORAGE__DATABASE__PATH` and `ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR` are no longer read.
