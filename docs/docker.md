# Container Usage

This document covers running `zainod` from the container image.

## Overview

The image runs `/entrypoint.sh`, which prepares a config file and then execs
`zainod start --config <file>`. The container:

- runs as the non-root user `container_user` (UID 1000, GID 1000) and refuses
  to start as root;
- exposes the lightwalletd-compatible gRPC server on port `8137` (plaintext);
- sources blocks from a Zebra node over Zebra's JSON-RPC interface, so Zebra
  must run with its `[rpc] listen_addr` set.

The entrypoint ignores container arguments; to run another `zainod`
subcommand, override it: `docker run --rm --entrypoint zainod zaino --help`.

## Configuration

The entrypoint picks the config file in this order:

1. `ZAINO_CONFIG_FILE` set: that file is used verbatim.
2. `/app/config/zainod.toml` exists (e.g. mounted): it is used.
3. Otherwise a config is written to `/app/config/zainod.toml` from the
   container env vars below.

| Variable                  | Default          | Meaning                                  |
| ------------------------- | ---------------- | ---------------------------------------- |
| `ZAINO_VALIDATOR_JSONRPC` | `127.0.0.1:8232` | Zebra JSON-RPC `host:port`               |
| `ZAINO_VALIDATOR_COOKIE`  | unset            | Path to Zebra's auth cookie, if enabled  |
| `ZAINO_STORE_PATH`        | `/app/data`      | LMDB index directory                     |
| `ZAINO_STORE_MAP_SIZE_GB` | `16`             | LMDB map size (maximum store size), GiB  |
| `ZAINO_GRPC_LISTEN`       | `0.0.0.0:8137`   | gRPC bind address                        |
| `ZAINO_CONFIG_FILE`       | unset            | Use this config file instead             |

These variables only feed the generated file and are unset before `zainod`
starts. Any other config key can still be overridden with `zainod`'s own
`ZAINO_`-prefixed variables, using `__` for nesting (for example
`ZAINO_INDEXER__BATCH_SIZE=500`). See
[`example_configs/zainod.toml`](./example_configs/zainod.toml) for the full
schema.

## Deployment with Docker Compose

```yaml
services:
  zaino:
    image: zaino:latest
    ports:
      - "8137:8137"   # gRPC
    volumes:
      - zaino-data:/app/data
    environment:
      - ZAINO_VALIDATOR_JSONRPC=zebra:8232
    depends_on:
      - zebra

  zebra:
    image: zfnd/zebra:latest
    volumes:
      - zebra-data:/home/zebra/.cache/zebra
    # ... zebra configuration; [rpc] listen_addr must be reachable from zaino

volumes:
  zaino-data:
  zebra-data:
```

To use a hand-written config instead, mount it read-only:

```yaml
    volumes:
      - ./config:/app/config:ro
      - zaino-data:/app/data
```

## Generating a Config File

```bash
mkdir -p ./config
docker run --rm --entrypoint zainod -v ./config:/app/config zaino \
  generate-config --output /app/config/zainod.toml
# edit ./config/zainod.toml
```

## Container Paths

| Purpose     | Mount Point   |
| ----------- | ------------- |
| Config      | `/app/config` |
| Index store | `/app/data`   |

These are symlinked to the XDG paths `zainod` uses by default
(`~/.config/zaino` and `~/.cache/zaino`).

## Volume Permissions

Mounted volumes must be writable by UID 1000. Named volumes are handled by the
container runtime. For bind mounts, set ownership before starting:

```bash
mkdir -p ./data
chown 1000:1000 ./data
```

## Health Check

The image's health check runs `zainod --version`; it confirms the binary runs,
not that the index is synced or the gRPC server is serving.

```bash
docker inspect --format='{{.State.Health.Status}}' <container>
```
