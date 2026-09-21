# zainod

`zainod` is the Zaino indexer daemon — an indexer for the Zcash blockchain,
written in Rust.

It sources blocks from a Zebra node over Zebra's JSON-RPC interface, builds a
persistent index (LMDB), and serves wallets over the
[lightclient protocol](https://github.com/zcash/lightwallet-protocol)
(`CompactTxStreamer` gRPC, the interface served by
[lightwalletd](https://github.com/zcash/lightwalletd)). The gRPC server is
plaintext; see [`docs/rpc_api.md`](https://github.com/zingolabs/zaino/blob/dev/docs/rpc_api.md)
for the methods served today.

This crate ships the `zainod` binary. The library half of the crate,
`zainodlib`, exposes the `run` entrypoint and configuration types for embedding
the daemon in other Rust programs.

For project background and architecture, see the
[Zaino repository](https://github.com/zingolabs/zaino).

## CLI

```text
zainod generate-config [--output FILE]   # write a default config file
zainod start [--config FILE]             # start the indexer
```

When `--config`/`--output` is omitted, the path defaults to
`$XDG_CONFIG_HOME/zaino/zainod.toml` (falling back to
`$HOME/.config/zaino/zainod.toml`).

Configuration is layered, highest priority first:

1. environment variables (prefix `ZAINO_`, `__` for nesting, e.g.
   `ZAINO_SOURCE__JSONRPC_ADDRESS=127.0.0.1:8232`),
2. the TOML config file,
3. built-in defaults.

The config has `[source]` (Zebra JSON-RPC address and auth), `[store]` (LMDB
path and map size), `[serve]` (gRPC listen address), `[indexer]` (sync tuning)
and an optional top-level `metrics_endpoint` (Prometheus, with the `prometheus`
feature). An annotated example lives at
[`docs/example_configs/zainod.toml`](https://github.com/zingolabs/zaino/blob/dev/docs/example_configs/zainod.toml).

## Launching

`zainod` needs a running Zebra node with JSON-RPC enabled (`[rpc] listen_addr`
in `zebrad.toml`), reachable at `source.jsonrpc_address`.

### From crates.io

```sh
cargo install zainod
zainod generate-config            # writes the default config, then edit it
zainod start                      # uses the default config path
# or point at an explicit file:
zainod start --config ./zainod.toml
```

### From source

```sh
git clone https://github.com/zingolabs/zaino.git
cd zaino
cargo run --release -p zainod -- start --config ./zainod.toml
```

### With Podman (rootless)

The container image runs as a non-root user (UID 1000) and refuses to start as
root, which makes it a natural fit for rootless Podman. Its entrypoint writes a
config from a small set of env vars unless one is mounted; see
[`docs/docker.md`](https://github.com/zingolabs/zaino/blob/dev/docs/docker.md).

```sh
podman run --rm \
  -p 8137:8137 \
  -e ZAINO_VALIDATOR_JSONRPC=host.containers.internal:8232 \
  -v zaino-data:/app/data \
  zainod:latest
```

`--userns=keep-id` maps the container's UID 1000 to your host user, so files in
mounted volumes stay owned by you:

```sh
podman run --rm --userns=keep-id \
  -p 8137:8137 \
  -v ./zainod.toml:/app/config/zainod.toml:ro,Z \
  -v zaino-data:/app/data \
  zainod:latest
```

A typical deployment runs `zainod` alongside Zebra with `podman compose`:

```yaml
services:
  zaino:
    image: zainod:latest
    ports:
      - "8137:8137"   # gRPC
    environment:
      - ZAINO_VALIDATOR_JSONRPC=zebra:8232
    volumes:
      - zaino-data:/app/data
    depends_on:
      - zebra

volumes:
  zaino-data:
```

```sh
podman compose up
```

## License

Apache-2.0.
