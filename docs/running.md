# Running zainod

`zainod` indexes the Zcash chain from a Zebra node over JSON-RPC and serves it to
light wallets over lightwalletd-compatible gRPC. The validator's JSON-RPC endpoint is
the only link between the two, so they can run on separate hosts. For the container
image see [docker.md](./docker.md), and for the methods zainod answers see
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
```

`generate-config` writes to the default config path unless given `-o <FILE>`.
`verify` is safe to run beside a live daemon, and its report and exit codes are
described in [`packages/zainod/usage.md`](../packages/zainod/usage.md#zainod-verify).

## Configuration

Configuration is layered with the highest priority first. Environment variables
prefixed `ZAINO_CONFIG_` win, with `__` separating nested keys, so
`ZAINO_CONFIG_FETCH__CONCURRENCY=64` sets `[fetch] concurrency`. The TOML
file comes next, then the built-in defaults. An unknown key fails the load.

[`example_configs/zainod.toml`](./example_configs/zainod.toml) annotates every key and
its default. In outline:

| Section | What it configures |
|---|---|
| `network` | `mainnet` (default), `testnet` or `regtest`. It is declared rather than read off the validator, because Zebra on regtest reports its chain as `"test"`. |
| `[metrics]` | The admin listener serving Prometheus `/metrics`, `/livez`, `/readyz` and `/statusz`: `listen_address` (unset by default, which disables it; `0.0.0.0` = every interface, so restrict who reaches it with the host firewall). |
| `[source]` | The validator's `jsonrpc_address` (default `127.0.0.1:8232`) and its credentials, either `cookie_path` or `user` and `password`. A cookie path takes precedence. `connect_timeout_secs` (2) and `read_timeout_secs` (30): the read timeout counts silence, not total duration, so a large block over a slow link completes. |
| `[[chainview_peers]]` | Extra validators, in the same shape as `[source]`, that the mempool view takes a quorum over. |
| `[serve]` | `grpc_listen_address` (default `127.0.0.1:8137`) and `max_address_rows` (100000). |
| `[grpc]` | Connection, stream and read caps, plus `trusted_proxies`. See [Network exposure](#network-exposure). |
| `[fetch]` | `finalised_depth` (1000), `concurrency` (32) and `primary_validator`, which pins bulk sync to one validator instead of spreading it across all of them. |
| `[index.*]` | One table per index, each with `enabled`, `path`, `batch_mib` (64) and `queue_mib` (256). |

The indexes are `compact_block`, `value_balance`, `block_hash`, `tree_state` and
`transparent_address`. Each `path` defaults to `$XDG_CACHE_HOME/zaino/indexes/<index>`
with the index name hyphenated. `compact_block` and `value_balance` cannot be
disabled, since the compact-block index reads its fees from the value-balance index.
`finalised_depth` below 1000 is refused on mainnet and testnet, because a reorg Zebra
accepts could then reach blocks zainod has already written. Only regtest may set less.

## A local Testnet pair

[`example_configs/`](./example_configs/) holds a zebrad config and a zainod config set
up against each other: zebrad serves JSON-RPC on `127.0.0.1:18232`, which is zainod's
`[source] jsonrpc_address`.

1. Install zebrad with `cargo install zebrad --locked`.
2. Replace `<PATH_TO_ZEBRA>` in `zebrad.toml` and `<ZAINO_DATA>` in `zainod.toml` with
   writable directories.
3. Run `zebrad -c docs/example_configs/zebrad.toml start`.
4. In another shell, run
   `cargo run --release -p zainod -- start -c docs/example_configs/zainod.toml`.

zainod then serves gRPC on `127.0.0.1:8137`.

## First launch

Each index syncs from genesis, so a first run on Mainnet or Testnet takes a long
time. Each index persists to its own directory and resumes from its own committed
height on restart.

**While an index is still building, every method it backs is refused with gRPC
`UNAVAILABLE`.** The exceptions are `GetTreeState`, `GetBlock` and `GetBlockRange`
(and its deprecated `GetBlockRangeNullifiers` alias), which answer any height their
index has already committed, since that answer is final and will not change. A range
reaching past the committed height is refused whole, never cut short. We refuse
rather than proxy to the validator because a derived answer comes from Zaino's own
index or not at all (see [design/boundaries.md](./design/boundaries.md)).
A client MUST read `UNAVAILABLE` as "retry later", never as "the chain ends here".

## Disk

Plan for the whole chain's worth of indexes on one local filesystem. We recommend
NVMe, because the indexes are read through mmap. For Mainnet in September 2026 we
estimate tens of GiB for compact blocks, about 27 GiB for the transparent-address
index at roughly 190M outputs, and under 1 GiB for tree state. These figures are not
yet measured on a full sync.

Every index file carries page checksums. A corrupt page stops zainod on the first
read that touches it. Run `zainod verify` to find the damaged index, then delete its
directory and let it resync.

## Stopping and restarts

Boot fails with exit code 1 if the validator does not answer, the gRPC address cannot
be bound, or an index cannot open. An index refuses to open when another zainod holds
its directory lock, when it was built for another network or format, when its files
are shorter than its `MANIFEST` seals (committed bytes were lost), or when its last
page fails its checksum.

Once booted, each stage (the index writers, the block producer, the chainview pollers
and the gRPC server) runs as its own task. SIGINT or SIGTERM cancels them all and
waits for each index to write what is final before exiting 0. If the producer, a
chainview poller or the gRPC server stops on its own, for example because a chainview
endpoint was ejected, it stops the rest the same way and zainod exits 1.

An index writer never stops with an error. A failure panics and aborts the process at
once, and so does a page checksum mismatch. A failed write names the index and its
directory, for example `compact_block index commit failed: disk
/home/zaino/.cache/zaino/indexes/compact_block full`. Free space or fix the disk, then
restart: every index resumes from its last committed batch.

zainod never restarts in-process, so run it under a service manager that restarts it
with backoff. Opening an index only reads file lengths and one tail page per file, so
a restarted zainod is serving again within seconds.

If the validators stop extending an index's chain, because of a reorg deeper than
`finalised_depth` or a validator that was reset or resynced onto another chain,
zainod stops with `ProduceError::Unlinked` or `ProduceError::Diverged`. Delete that index's
directory to resync it.

## Network exposure

zainod reaches the validator over plain HTTP JSON-RPC. Its gRPC listener serves
plaintext HTTP/2 unless `[serve.tls]` names a certificate pair, in which case zainod
terminates TLS itself (rustls with the ring provider, ALPN `h2`). Expose the listener
beyond a trusted network only with `[serve.tls]` or behind a TLS-terminating proxy.

```toml
[serve.tls]
cert_path = "/etc/zaino/fullchain.pem"   # chain, leaf first
key_path = "/etc/zaino/key.pem"          # PKCS#8, PKCS#1 or SEC1
```

- A missing, unreadable or mismatched pair fails startup.
- Both files are checked every minute. A changed pair is swapped in for new connections
  without a restart, so an ACME client renewing them in place needs no hook. A changed
  pair that fails to load is logged and the previous one keeps serving.
- A client that does not finish its handshake within 10 seconds is dropped.
- With a proxy in front that sends a PROXY header, the header precedes the handshake.

The `[grpc]` caps, listed with their defaults in the example config, all refuse
rather than queue. A connection over a cap is closed at accept and a stream over one
is answered `UNAVAILABLE` with a retry hint. The read lanes are the exception: an
index read waits for a permit in its own lane.

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

If `accept()` still fails at runtime with `EMFILE`, `ENOBUFS` or a similar error, the
listener backs off from 10 ms, doubling up to 1 s, and keeps serving the connections
it already has. The `zaino_grpc_accept_errors_total` metric counts each failure.

## Logging

Logging is configured from the environment through `RUST_LOG`, `ZAINOLOG_FORMAT`,
`ZAINOLOG_COLOR` and `ZAINOLOG_LOCATION`, which are described in
[`packages/zainod/usage.md`](../packages/zainod/usage.md#logging).
