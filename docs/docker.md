# Container image

Built from the repo-root `Dockerfile`. The image:

- runs `zainod start` as `container_user` (UID/GID 1000)
- reads its config from `/app/config/zainod.toml`, the same file and schema as
  any other install
- exposes the plaintext gRPC server on port `8137`
- needs a Zebra node with `[rpc] listen_addr` reachable from the container

There is no container-only configuration: mount a `zainod.toml` and the image
runs it. Arguments replace `start`, e.g.
`docker run zaino verify` checks the indexes the mounted config names. zainod's
own `ZAINO_CONFIG_` variables (`__` for nesting) override any key of the file,
e.g. `ZAINO_CONFIG_NETWORK=testnet`. Full schema:
[`example_configs/zainod.toml`](./example_configs/zainod.toml).

## Paths

| Purpose | Mount point | Symlinked from |
|---|---|---|
| Config | `/app/config` | `~/.config/zaino` (zainod's default config path) |
| Index data | `/app/data` | `~/.cache/zaino` (every index's default directory) |

A config that sets no `[index.*] path` keeps every index under `/app/data`.
Mounted volumes must be writable by UID 1000. For a bind mount:
`mkdir -p ./data && chown 1000:1000 ./data`. Under rootless Podman,
`--userns=keep-id` maps UID 1000 to your host user.

## Compose

```yaml
services:
  zaino:
    image: zaino:latest
    ports:
      - "8137:8137"
    volumes:
      - ./config:/app/config:ro   # zainod.toml: [source] jsonrpc_address = "zebra:8232"
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

To generate a config to edit:

```sh
mkdir -p ./config
docker run --rm -v ./config:/app/config zaino generate-config
```

## Health check

`zainod --version`: proves the binary runs, not that an index is synced or the
gRPC server is serving.
