# Container image

The repo-root `Dockerfile` builds a Debian bookworm image whose entrypoint is
`zainod` and whose default command is `start`. It runs as `container_user` (UID and
GID 1000), reads its config from `/app/config/zainod.toml`, and exposes the gRPC port
`8137`.

```sh
docker build -t zaino .
docker build -t zaino --build-arg CARGO_PROFILE=profiling .   # line tables and frame pointers
docker build -t zaino --build-arg CARGO_FEATURES=snapshot .   # [snapshot] bootstrap (+ aria2c)
```

There is no container-only configuration. The image reads the same `zainod.toml` as
any other install ([`example_configs/zainod.toml`](./example_configs/zainod.toml)
annotates the full schema), and `ZAINO_CONFIG_` variables override any key of it, with
`__` separating nested keys, so `-e ZAINO_CONFIG_NETWORK=testnet` works as it does on
the host. Arguments replace `start`, so `docker run zaino verify` checks the indexes
the mounted config names.

The default `grpc_listen_address` is `127.0.0.1:8137`, which a published port cannot
reach. Inside a container the config MUST set it to `0.0.0.0:8137`, either in the file
or with `ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS=0.0.0.0:8137`. The same applies to
`[metrics] listen_address` if you scrape it from outside the container. zainod then logs a
warning, because `/metrics` is unauthenticated. The validator's `[rpc] listen_addr`
must likewise be reachable from the container.

## Paths

The image mounts its config and data under `/app` and symlinks zainod's XDG default
paths to them, so a config that sets no `[index.*] path` keeps every index under
`/app/data`.

| Purpose | Mount point | Symlinked from |
|---|---|---|
| Config | `/app/config` | `~/.config/zaino`, zainod's default config directory |
| Index data | `/app/data` | `~/.cache/zaino`, the parent of every index's default directory |

Mounted volumes must be writable by UID 1000. For a bind mount, run
`mkdir -p ./data && chown 1000:1000 ./data`. Under rootless Podman,
`--userns=keep-id` maps UID 1000 to your host user.

## Compose

```yaml
services:
  zaino:
    image: zaino:latest
    ports:
      - "8137:8137"
    environment:
      ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS: "0.0.0.0:8137"
    volumes:
      - ./config:/app/config:ro   # zainod.toml with [source] jsonrpc_address = "zebra:8232"
      - zaino-data:/app/data
    depends_on:
      - zebra

  zebra:
    image: zfnd/zebra:latest
    volumes:
      - zebra-data:/home/zebra/.cache/zebra
    # [rpc] listen_addr must be reachable from zaino

volumes:
  zaino-data:
  zebra-data:
```

`jsonrpc_address` accepts a hostname, which is resolved on each connection, so a
Compose service name works. To generate a config to edit:

```sh
mkdir -p ./config
docker run --rm -v ./config:/app/config zaino generate-config
```

## Health check

The image's `HEALTHCHECK` runs `zainod --version` every 30 seconds. It proves the
binary runs, not that an index is synced or that the gRPC server is serving. For a
liveness probe that tracks the serving runtime, set `[metrics] listen_address` and probe
`/livez` (see [`packages/zainod/usage.md`](../packages/zainod/usage.md#the-admin-listener)).
