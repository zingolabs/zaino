# Container image

The repo-root `Dockerfile` builds `zainod` as a static musl binary (jemalloc allocator) on Alpine.
It runs as `zaino` (UID and GID 1000), listens for gRPC on `0.0.0.0:8137`, and has a built-in
health check. Entrypoint `zainod`, default command `start`.

```sh
docker build -t zainod .
docker build -t zainod --build-arg CARGO_PROFILE=profiling .   # line tables and frame pointers
```

Both base images are pinned by digest and the Alpine packages on them are not, so a rebuild of
the same commit installs the same packages and a digest bump picks up security fixes. The image
ships `aria2c` (the `[snapshot]` bootstrap's downloader), CA certificates and busybox.

## Run

```sh
docker run --rm zainod generate-config -o /dev/stdout > zainod.toml   # every key at its default
# edit zainod.toml: network, [[trusted_validators]]
docker run -d --name zainod -p 8137:8137 \
  -v ./zainod.toml:/etc/zaino/zainod.toml:ro \
  -v zainod-data:/var/lib/zaino \
  zainod
```

Every other subcommand reads the same config and data: `docker exec zainod zainod hold` beside
the running node, or `zainod verify` in a one-off container with the same two mounts.

## Paths

| Path                      | Holds                                                        | Mount                     |
| ------------------------- | ------------------------------------------------------------ | ------------------------- |
| `/etc/zaino/zainod.toml`  | the config (zainod's default path)                           | a file, read-only         |
| `/etc/zaino/tls/`         | the `[serve.tls]` and `indexer_tls` PEM files, by convention | a directory, read-only    |
| `/var/lib/zaino/`         | `indexes/`, `state/` (the `zainod hold` marker), `peers/`    | a volume, writable by 1000 |

The image sets `XDG_CONFIG_HOME=/etc` and `XDG_CACHE_HOME=XDG_DATA_HOME=/var/lib`, so a config
that names no path keeps everything zainod writes under `/var/lib/zaino`. A named volume takes the
image's ownership on first use. A bind mount must be writable by UID 1000:
`mkdir -p ./data && chown 1000:1000 ./data`. Under rootless Podman, `--userns=keep-id` maps UID
1000 to your host user.

## Configure

The image reads the same `zainod.toml` as any other install, layered under `ZAINO_CONFIG_*`
environment variables ([running.md: Configuration](./running.md#configuration)). The file is
required: `[[trusted_validators]]` is an array of tables, which environment variables cannot
express. Everything else can come from either place:

```sh
docker run ... -e ZAINO_CONFIG_SYNC__CONCURRENCY=64 -e RUST_LOG=info,zaino_grpc=debug zainod
```

The image's one default that differs from a host install is
`ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS=0.0.0.0:8137` (zainod's own default, `127.0.0.1`, is
unreachable through a published port). To scrape `/metrics`, set
`ZAINO_CONFIG_METRICS__LISTEN_ADDRESS=0.0.0.0:9999`: zainod warns at boot that it binds a
non-private address, so publish that port only to the scraper
([running.md: Network exposure](./running.md#network-exposure)).

## TLS

Mount the certificate directory, not the files. zainod re-reads the pair every 60 s, and a
single-file bind mount keeps the old inode when a renewal replaces the file, so the new
certificate would never arrive.

```sh
docker run ... \
  -v /etc/letsencrypt/live/zaino.example.com:/etc/zaino/tls:ro \
  -e ZAINO_CONFIG_SERVE__TLS__CERT_PATH=/etc/zaino/tls/fullchain.pem \
  -e ZAINO_CONFIG_SERVE__TLS__KEY_PATH=/etc/zaino/tls/privkey.pem \
  zainod
```

The key must be readable by UID 1000. The same directory holds the client certificates for a
validator's `[trusted_validators.indexer_tls]`, named in `zainod.toml`.

## Health check

The built-in check GETs the public `/readyz` on `127.0.0.1:8137`, over plaintext or, with
`[serve.tls]`, over HTTPS. It passes while zainod is `serving` or `degraded`, so a fresh node
reports unhealthy until its indexes reach the tip
([states](../packages/zainod/usage.md#the-admin-listener)). With the gRPC listener on another
port, override it:

```yaml
healthcheck:
  test: ["CMD", "wget", "-q", "-O", "/dev/null", "http://127.0.0.1:9137/readyz"]
```

## Compose

```yaml
services:
  zainod:
    image: zingodevops/zainod:latest
    ports:
      - "8137:8137"
    volumes:
      - ./zainod.toml:/etc/zaino/zainod.toml:ro   # [[trusted_validators]] jsonrpc_address = "zebra:8232"
      - zainod-data:/var/lib/zaino
    depends_on:
      - zebra

  zebra:
    image: zfnd/zebra:latest
    volumes:
      - zebra-data:/home/zebra/.cache/zebra
    # [rpc] listen_addr must be reachable from zainod

volumes:
  zainod-data:
  zebra-data:
```

`jsonrpc_address` accepts a hostname, resolved on each connection, so a Compose service name
works.
