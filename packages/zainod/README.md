# zainod

`zainod` is the Zaino indexer daemon — an indexer for the Zcash blockchain,
written in Rust.

It sits between a Zcash full validator (Zebra or Zcashd) and client
applications, serving:

- the [lightclient protocol API](https://github.com/zcash/lightwallet-protocol), the interface today
  served by [lightwalletd](https://github.com/zcash/lightwalletd), and
- a **JSON-RPC API** covering the subset of Zcash RPCs needed by wallets and
  block explorers.

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

1. environment variables (prefix `ZAINO_`),
2. the TOML config file,
3. built-in defaults.

Sensitive fields (passwords, secrets, tokens, cookies, private keys) cannot be
set via environment variables and must come from the config file.

## Launching

`zainod` needs a running validator to connect to. The examples below assume one
is reachable at the address in your config.

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

The daemon is published as a container image with `zainod start` as the default
command. It runs as a non-root user (UID 1000) and refuses to start as root,
which makes it a natural fit for rootless Podman.

Run it directly, mounting a config file and a data volume:

```sh
podman run --rm \
  -p 8137:8137 \
  -p 8237:8237 \
  -v ./zainod.toml:/app/config/zainod.toml:ro,Z \
  -v zaino-data:/app/data \
  zainod:latest
```

`--userns=keep-id` maps the container's UID 1000 to your host user, so files in
the mounted data volume stay owned by you:

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
      - "8237:8237"   # JSON-RPC (if enabled)
    volumes:
      - ./config:/app/config:ro,Z
      - zaino-data:/app/data
    depends_on:
      - zebra

volumes:
  zaino-data:
```

```sh
podman compose up
```

See [`docs/docker.md`](https://github.com/zingolabs/zaino/blob/dev/docs/docker.md)
for the full container guide.

## License

Apache-2.0.
