# Running zainod

`zainod` indexes the Zcash chain from a Zebra node over JSON-RPC and serves it to
light wallets over lightwalletd-compatible gRPC. For the container image see
[docker.md](./docker.md); for the methods it answers see [rpc_api.md](./rpc_api.md).

## Requirements

A [Zebra](https://github.com/ZcashFoundation/zebra) node with JSON-RPC enabled
(`[rpc] listen_addr` in `zebrad.toml`). The live suite runs zebrad 6.2.3. That
endpoint is the only link between the two, so they may run on separate hosts.

## Install and run

```sh
cargo build --release -p zainod          # target/release/zainod
zainod generate-config                   # every key at its default
zainod start                             # $XDG_CONFIG_HOME/zaino/zainod.toml
zainod start -c /path/to/zainod.toml
zainod verify -c /path/to/zainod.toml    # read-only page-checksum scrub, see packages/zainod/usage.md
```

Configuration is layered highest-priority-first: `ZAINO_CONFIG_`-prefixed
environment variables (`__` for nesting, e.g.
`ZAINO_CONFIG_FETCH__FINALISED_DEPTH=100`), then the TOML file, then the
built-in defaults. Unknown keys fail the load. An annotated config is
[`example_configs/zainod.toml`](./example_configs/zainod.toml); index
directories default to `$XDG_CACHE_HOME/zaino/indexes/<index>`.

## A local Testnet pair

[`example_configs/`](./example_configs/) holds a zebrad and a zainod config set
up against each other: zebrad serves JSON-RPC on `127.0.0.1:18232`, zainod's
`source.jsonrpc_address`.

1. Install zebrad: `cargo install zebrad --locked`.
2. Replace `<PATH_TO_ZEBRA>` in `zebrad.toml` and `<ZAINO_DATA>` in
   `zainod.toml` with writable directories.
3. `zebrad -c docs/example_configs/zebrad.toml start`
4. In another shell:
   `cargo run --release -p zainod -- start -c docs/example_configs/zainod.toml`

zainod then serves gRPC on `127.0.0.1:8137`.

## First launch

Each index syncs from genesis, so a first run on Mainnet or Testnet takes a long
time. Each index persists to its own `[index.*] path` and resumes from its own
committed height on restart.

**While an index is still building, every method it backs is refused with gRPC
`UNAVAILABLE`**, with one exception: `GetTreeState` answers any height the
tree-state index has already committed (final, so the answer never changes). A
derived answer comes from Zaino's index or not at all (see
[design/boundaries.md](./design/boundaries.md)). A client reads `UNAVAILABLE` as
"retry later", never as "the chain ends here".

## Disk

Plan for the chain's worth of indexes on one local filesystem (NVMe
recommended; the indexes are read with mmap). Mainnet, Sep 2026 (estimates, not
measured on a full sync yet): compact blocks tens of GiB, the transparent-address
index ~27 GiB across its two segment sets at ~190M outputs, tree state under 1 GiB. Every index file carries page
checksums; a corrupt page stops zainod on the read that touches it (run
`zainod verify`, then delete and resync that index).

## Stopping and restarts

Boot fails (exit 1) if a validator does not answer, the gRPC address cannot be
bound, or an index cannot open. An index refuses to open if its directory is
locked by another zainod, was built for another network or format, or its files
are shorter than its `MANIFEST` seals (lost committed bytes) or its last page
fails its checksum.

Once booted, each stage (index writers, the block producer, chainview pollers,
gRPC server) runs as its own task. SIGINT/SIGTERM cancels them all and waits for
each index to write what is final before exiting 0. Any task stopping on its own
(an error, or a chainview endpoint being ejected) stops the rest the same way
and exits 1; a panic (including a page checksum mismatch) aborts the process at
once. zainod never restarts in-process: run it under a service manager that
restarts it with backoff. Opening is cheap (lengths and one tail page per file),
so a restart serves again in seconds.

An index whose chain the validators no longer extend (a reorg below
`finalised_depth`, a validator reset or resynced onto another chain) stops
zainod with `FollowError::Unlinked`; delete that index's directory to resync it.

## Network exposure

zainod links no TLS stack and serves plaintext HTTP/2. Expose the gRPC listener
beyond a trusted network only behind a TLS-terminating proxy. The validator
connection is plain HTTP JSON-RPC.

### Behind a proxy

`[grpc] max_connections_per_ip` caps what one client may hold. Behind a proxy
every connection comes from the proxy's address, so without more the cap applies
to the whole server. List the proxy in `trusted_proxies` and have it send a
[PROXY protocol](https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt)
header (v1 or v2) naming the client:

```toml
[grpc]
trusted_proxies = ["10.0.0.5/32"]
```

- A connection from a trusted address **must** open with the header, or it is
  closed. Connections from other addresses are served as before, without one.
- nginx: terminate TLS in the `stream` module and pass the decrypted h2c through
  with `proxy_protocol on;`. The `http` module's `grpc_pass` cannot send the
  header.

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

- HAProxy: `server zaino 127.0.0.1:8137 send-proxy-v2` in a `mode tcp` backend.
- A proxy that cannot send the header (e.g. nginx `grpc_pass`): leave
  `trusted_proxies` empty and raise `max_connections_per_ip` to
  `max_connections`, since the cap cannot tell its clients apart.

### Open files

Every connection is a file descriptor. At boot zainod raises its soft
`RLIMIT_NOFILE` to the hard limit, then refuses to start (exit 1) if
`max_connections` plus 1024 (reserved for index files and the validator) does
not fit. Raise the hard limit (systemd `LimitNOFILE=`, docker
`--ulimit nofile=`) or lower `max_connections`. If `accept()` still fails at
runtime (`EMFILE`, `ENOBUFS`, ...), the listener backs off (10 ms doubling to
1 s) and keeps serving the open connections; `zaino_grpc_accept_errors_total`
counts each.

## Logging

Configured from the environment (`RUST_LOG`, `ZAINOLOG_FORMAT`,
`ZAINOLOG_COLOR`, `ZAINOLOG_LOCATION`); see
[`packages/zainod/usage.md`](../packages/zainod/usage.md#logging).
