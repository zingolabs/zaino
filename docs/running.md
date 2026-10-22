# Running zainod

`zainod` indexes the Zcash chain from one or more trusted Zebra validators and serves it to
light wallets over lightwalletd-compatible gRPC. It reaches each validator over JSON-RPC (or, for blocks, its indexer gRPC:
[`validator_client`](../packages/zainod/usage.md#validator-client)), and with
`[p2p]` on it also takes checked headers, blocks and mempool listings from Zcash peers
([architecture §2–3](./architecture.md#2-the-map)). Validators can run on separate hosts. For
the container image see [docker.md](./docker.md), and for the methods zainod answers see
[rpc_api.md](./rpc_api.md).

## Requirements

zainod needs a [Zebra](https://github.com/ZcashFoundation/zebra) node with JSON-RPC
enabled through `[rpc] listen_addr` in `zebrad.toml`. The live suite tests against
zebrad 6.2.3.

## Install and run

```sh
cargo build --release -p zainod          # produces target/release/zainod
zainod generate-config                   # writes every key at its default
zainod start                             # reads $XDG_CONFIG_HOME/zaino/zainod.toml
zainod start -c /path/to/zainod.toml
zainod verify -c /path/to/zainod.toml    # read-only page-checksum scrub
zainod hold -c /path/to/zainod.toml      # out of rotation until `zainod resume`
```

`-c, --config <FILE>` works before or after any subcommand. `generate-config` writes to
`-o <FILE>` if given, else to the `--config` path, else to the default config path.
[`verify`](../packages/zainod/usage.md#zainod-verify) is safe beside a live daemon;
[`hold` / `resume`](../packages/zainod/usage.md#operator-hold) drain a node behind a load
balancer.

## Configuration

Configuration is layered with the highest priority first. Environment variables
prefixed `ZAINO_CONFIG_` win, with `__` separating nested keys, so
`ZAINO_CONFIG_SYNC__CONCURRENCY=64` sets `[sync] concurrency`. The TOML
file comes next, then the built-in defaults. An unknown key fails the load.

[`example_configs/zainod.toml`](./example_configs/zainod.toml) documents every key and its
default. In outline:

| Key or table             | For                                                                                         |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| `network`                | `mainnet` (default), `testnet` or `regtest`, declared rather than read off the validator    |
| `validator_client`       | `legacy_rpc` (default) or `grpc` ([zainod: Validator client](../packages/zainod/usage.md#validator-client)) |
| `[metrics]`              | the admin listener's `listen_address` (unset = off; see [Network exposure](#network-exposure)) |
| `[[trusted_validators]]` | one table per validator: address, credentials, timeouts, link budget, `indexer_address` and `[trusted_validators.indexer_tls]`, `priority` tier ([architecture §3](./architecture.md#3-sources-and-what-each-is-trusted-for)) |
| `[submission]`           | when `SendTransaction` relays again ([architecture §6](./architecture.md#6-submission))    |
| `[p2p]`                  | Zaino's own Zcash peers, off by default                                                     |
| `[serve]`                | gRPC listen address, `max_address_rows`, `[serve.tls]`, `state_dir`, `hold_goaway_delay_secs` |
| `[grpc]`                 | connection, stream and read caps, connection lifetimes, `trusted_proxies`, `shutdown_grace_secs` |
| `[sync]`                 | the shared block-fetch pipeline; `[sync.lsm_store]` = the disk engine's merge budget       |
| `[index.<name>]`         | `compact_block`, `block_hash`, `tree_state`, `transparent_address`, `txid_location`: `enabled`, `path` (at least one on; `utxo_set` comes with `compact_block`, beside its `path`) |
| `[snapshot]`             | fill empty indexes from an [index snapshot](../packages/zainod/usage.md#index-snapshot) (`aria2c` on `PATH`) |

There is no vote among trusted validators: each one added adds failover, so two is the
first set that survives one being down.

## A Public Testnet pair

[`example_configs/`](./example_configs/) holds a zebrad config and a zainod config for the
Public Testnet, set up against each other: zebrad serves JSON-RPC on `127.0.0.1:18232`, which
is zainod's `[[trusted_validators]] jsonrpc_address`.

1. Install zebrad with `cargo install zebrad --locked`.
1. Replace `<PATH_TO_ZEBRA>` in `zebrad.toml` and `<ZAINO_DATA>` in `zainod.toml` with
   writable directories.
1. Run `zebrad -c docs/example_configs/zebrad.toml start`.
1. In another shell, run
   `cargo run --release -p zainod -- start -c docs/example_configs/zainod.toml`.

zainod then serves gRPC on `127.0.0.1:8137`.

## First launch

Each index syncs from genesis (or from an [index snapshot](../packages/zainod/usage.md#index-snapshot)),
so a first run on Mainnet or Testnet takes a long time, and resumes from its own durable height
on restart. It serves while it syncs
([what each method answers meanwhile](./rpc_api.md#where-each-method-is-answered)).

## Disk

Plan for the whole chain's worth of indexes on local storage, preferably NVMe, because the
indexes are read through mmap. Every index file carries page checksums: run `zainod verify` to
find a damaged index, then delete its directory and let it resync.

## Stopping and restarts

Boot fails with exit code 1 if a `[[trusted_validators]]` entry cannot make a client (checked
first), the gRPC address cannot be bound (next, before any index-snapshot download) or an index
cannot open. On mainnet and testnet no validator needs to answer for boot to finish (regtest
reads its upgrade schedule from the first validator that answers, and refuses one listed out of
protocol order or without Sapling). A validator that goes away is retried
([architecture §9](./architecture.md#9-talking-to-validators-and-peers)).

SIGINT or SIGTERM closes the gRPC listener at once, sends every open connection an HTTP/2
GOAWAY, waits up to `[grpc] shutdown_grace_secs` for in-flight streams, lets each index commit
what it holds, and exits 0. The same holds during boot: a signal while an index snapshot
downloads (aria2c stopped with it) or while regtest waits on its first validator exits 0.
Behind a load balancer that routes by polling `/readyz`, run
[`zainod hold`](../packages/zainod/usage.md#operator-hold) first, then stop. Under systemd, use
[`Type=notify`](../packages/zainod/usage.md#systemd) so a restart waits until zainod serves again.

zainod never restarts in-process and stops on any fault it cannot vouch past
([failure policy](../packages/zainod/usage.md#failure-policy)), so run it under a service
manager that restarts it with backoff. Opening an index checks each file's length, its tail
page and its `.crc` checksum file (1/1024 of the data), never the data pages, so a restarted
zainod is serving again within seconds, after the NFS fetches every block above the durable tips
again (progress logged as `Fetching non-finalized blocks`).

## Status

The admin listener's `/readyz` and `/metrics` gauges, and the public `/readyz` and `/status` on
the gRPC listener, each read one published `GrpcSnapshot` of what zainod serves
([architecture §8](./architecture.md#8-serving-one-grpcsnapshot)). States, readiness rules and every
field: [zainod: the admin listener](../packages/zainod/usage.md#the-admin-listener).

## Network exposure

zainod listens on two interfaces:

- **gRPC** (`[serve] grpc_listen_address`): plaintext HTTP/2 unless `[serve.tls]` names a
  certificate pair, in which case zainod terminates TLS itself (rustls with the ring provider).
  Expose it beyond a trusted network only with `[serve.tls]` or behind a
  TLS-terminating proxy. Its public `/readyz` and `/status` are safe for the open internet.
- **Admin** (`[metrics] listen_address`, off by default): `/metrics`, `/livez` and `/readyz` with
  no authentication or encryption; the metrics publish the chain tip, sync progress, traffic and
  validator addresses. zainod warns at startup when it binds a non-private address.
  Keep it on loopback, a private interface, or a network only the scraper reaches.

Outbound, each validator link is plain HTTP JSON-RPC, and its indexer gRPC is plaintext unless
`[trusted_validators.indexer_tls]` sets up mutual TLS.

```toml
[serve.tls]
cert_path = "/etc/zaino/fullchain.pem"   # chain, leaf first
key_path = "/etc/zaino/key.pem"          # PKCS#8, PKCS#1 or SEC1
```

A bad pair fails startup. A renewed pair is picked up within a minute with no restart, so an
ACME client renewing in place needs no hook; one that fails to load keeps the previous pair
serving. The TLS details: [zaino-grpc: serve stack](../packages/zaino-grpc/usage.md#serve-stack).

The `[grpc]` caps, listed with their defaults in the example config, refuse rather than
queue, except the read lanes. How each cap, the accept loop and the health paths behave:
[zaino-grpc: serve stack](../packages/zaino-grpc/usage.md#serve-stack).

### Stream time limits

No stream has a cap on its total length, so a full sync streams for as long as it keeps
receiving. Two `[grpc]` bounds catch a stream that makes no progress: `unread_timeout_secs` (its
client stopped reading: the connection closes) and `silent_timeout_secs` (nothing to send: the
stream ends `UNAVAILABLE`) ([zaino-grpc: serve stack](../packages/zaino-grpc/usage.md#serve-stack)).

### Behind a proxy

Behind a proxy every connection comes from the proxy's address, so
`max_connections_per_ip` would cap the whole server rather than each client. To keep
the cap per client, list the proxy in `trusted_proxies` and have it send a
[PROXY protocol](https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt) header
(v1 or v2) naming the client:

```toml
[grpc]
trusted_proxies = ["10.0.0.5/32"]
```

A connection from a trusted address MUST open with the header, or it is closed.
Connections from any other address are served without one.

With nginx, terminate TLS in the `stream` module and pass the decrypted h2c through
with `proxy_protocol on;`. The `http` module's `grpc_pass` cannot send the header.

```nginx
stream {
    server {
        listen 443 ssl;
        ssl_certificate     /etc/ssl/zaino.pem;
        ssl_certificate_key /etc/ssl/zaino.key;
        ssl_alpn            h2;
        proxy_pass          127.0.0.1:8137;
        proxy_protocol      on;
    }
}
```

With HAProxy, use `server zaino 127.0.0.1:8137 send-proxy-v2` in a `mode tcp`
backend.

If your proxy cannot send the header, as with nginx `grpc_pass`, leave
`trusted_proxies` empty and raise `max_connections_per_ip` to `max_connections`,
because the cap cannot tell the proxy's clients apart.

### Open files

Every connection is a file descriptor. At boot zainod raises its soft `RLIMIT_NOFILE`
to the hard limit, then refuses to start with exit code 1 if `max_connections` plus
1024 descriptors reserved for index files and the validator does not fit. Raise the
hard limit (systemd `LimitNOFILE=`, docker `--ulimit nofile=`) or lower
`max_connections`.

## Logging

Logging is configured from the environment through `RUST_LOG`, `ZAINOLOG_FORMAT`,
`ZAINOLOG_COLOR` and `ZAINOLOG_LOCATION`, which are described in
[`packages/zainod/usage.md`](../packages/zainod/usage.md#logging).
