# Running zainod

`zainod` indexes the Zcash chain from one or more trusted Zebra validators and serves it to
light wallets over lightwalletd-compatible gRPC. With `[p2p]` on it also takes checked headers,
blocks and mempool listings from Zcash peers ([architecture §2–3](./architecture.md#2-the-map)).
The container image: [docker.md](./docker.md). The methods it answers: [rpc_api.md](./rpc_api.md).

## Requirements

A [Zebra](https://github.com/ZcashFoundation/zebra) node with JSON-RPC enabled through
`[rpc] listen_addr` in `zebrad.toml`. The live suite tests against zebrad 6.2.3.

## Install and run

```sh
cargo build --release -p zainod          # produces target/release/zainod
zainod generate-config                   # writes every key at its default
zainod start                             # reads $XDG_CONFIG_HOME/zaino/zainod.toml
zainod start -c /path/to/zainod.toml
zainod verify                            # read-only page-checksum scrub
zainod hold                              # out of rotation until `zainod resume`
zainod overload off                      # override the overload detector
```

`-c, --config <FILE>` works before or after any subcommand; `zainod --help` describes each.

## Configuration

Environment variables prefixed `ZAINO_CONFIG_` win, with `__` separating nested keys
(`ZAINO_CONFIG_SYNC__CONCURRENCY=64` sets `[sync] concurrency`); then the TOML file; then the
built-in defaults. An unknown key, or any value `zainod start` refuses, fails the load for every
subcommand. Default paths sit under the user's XDG base
directories; with no home directory (e.g. `HOME` unset under a service manager) the load fails
naming each path to set: `serve.state_dir`, every enabled `index.<name>.path`, and
`p2p.cache_dir` when `[p2p]` is enabled (and `--config` for the file itself).
[`example_configs/zainod.toml`](./example_configs/zainod.toml) documents every key and its
default. In outline:

| Key or table             | For                                                                                         |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| `network`                | `mainnet` (default), `testnet` or `regtest`, declared rather than read off the validator    |
| `validator_client`       | `legacy_rpc` (default) or `grpc` ([below](#validator-client))                               |
| `[metrics]`              | the admin listener's `listen_address` (unset = off; see [Network exposure](#network-exposure)) |
| `[[trusted_validators]]` | one table per validator: address, credentials, timeouts, link budget, `indexer_address`, `priority` tier |
| `[submission]`           | when `SendTransaction` relays again ([architecture §6](./architecture.md#6-submission))    |
| `[p2p]`                  | Zaino's own Zcash peers, off by default                                                     |
| `[serve]`                | gRPC listen address, `[serve.tls]`, `state_dir`, `hold_goaway_delay_secs`                   |
| `[grpc]`                 | connection, stream and read caps, connection lifetimes, `trusted_proxies`, `[grpc.overload]` |
| `[sync]`                 | the shared block-fetch pipeline and finality depth                                          |
| `[index.<name>]`         | per index: `enabled`, `path` (at least one on; `utxo_set` comes with `compact_block`)        |
| `[snapshot]`             | fill empty indexes from an [index snapshot](#index-snapshot)                                |

There is no vote among trusted validators: each one added adds failover, so two is the first set
that survives one being down.

### Validator client

`validator_client` picks one client type for every `[[trusted_validators]]` entry:

- `legacy_rpc` (default): JSON-RPC for everything, zebrad or zcashd.
- `grpc`: blocks over zebrad's indexer gRPC at each entry's `indexer_address`, the rest over
  JSON-RPC. It needs a zebrad whose indexer serves `GetBlockRange` (Zaino's zebra fork; stock
  zebrad lacks it). An entry without `indexer_address` fails boot.

The indexer gRPC is plaintext unless `[trusted_validators.indexer_tls]` sets up mutual TLS.

## A Public Testnet pair

[`example_configs/`](./example_configs/) holds a zebrad and a zainod config for the Public
Testnet, set up against each other (zebrad's JSON-RPC on `127.0.0.1:18232`).

1. Install zebrad with `cargo install zebrad --locked`.
1. Replace `<PATH_TO_ZEBRA>` in `zebrad.toml` and `<ZAINO_DATA>` in `zainod.toml` with writable
   directories.
1. Run `zebrad -c docs/example_configs/zebrad.toml start`.
1. In another shell, run `cargo run --release -p zainod -- start -c docs/example_configs/zainod.toml`.

zainod then serves gRPC on `127.0.0.1:8137`.

## First launch and disk

Each index syncs from genesis (or from an [index snapshot](#index-snapshot)), so a first run on
Mainnet or Testnet takes a long time; a restart resumes from each index's durable height. It
serves while it syncs ([what each method answers meanwhile](./rpc_api.md#where-each-method-is-answered)).

Plan for the whole chain's worth of indexes on local storage, preferably NVMe: the indexes are
read through mmap. Every index file carries page checksums: [`zainod verify`](#zainod-verify)
finds a damaged index, then delete its directory and let it resync.

## Index snapshot

With `aria2c` on `PATH` (the Docker image and the Nix package ship it), zainod fills empty
indexes from a published index snapshot before it boots:

```toml
[snapshot]
manifest = "https://snapshots.zingolabs.dev/zaino-snapshot-1.1.0/manifest.json"
connections = 8   # total across every archive, at most 16
```

The manifest, `{"network", "height", "indexes": {<index>: {"archive", "sha256", "archive_bytes",
"installed_bytes"}}}`, lists one archive per index, all from one seed at one `height`. Each
archive is a zstd tar of that index directory's contents (no top-level directory); `archive`
resolves against the manifest URL, `installed_bytes` = its unpacked size.

- Only an enabled index whose directory is missing or empty is filled; one holding data is never
  touched. With none empty nothing is downloaded. An empty index the manifest has no archive for
  stops startup.
- Free space is checked per filesystem before any download; too little stops startup and names
  the directory and the bytes needed.
- Downloads retry transient errors without limit, and a restart resumes each archive at the step
  it stopped in (download, unpack, install). A manifest for another network, a hash mismatch or a
  permanent download error stops startup.

While it runs the state is `provisioning`, with its progress in `detail`.

## States

zainod reports one state, the same on every surface: `/readyz` (admin and gRPC listeners), the
public `/status`, the `zaino_state{state}` gauge and the `State changed` log line. Each reads one
`GrpcSnapshot` ([architecture §8](./architecture.md#8-serving-one-grpcsnapshot)). The first rule
that matches wins:

| `state`        | When                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------- |
| `held`         | an operator ran [`zainod hold`](#operator-hold)                                          |
| `down`         | the serving runtime stalled (no heartbeat for 30 s), boot waits on its first validator answer, [overloaded](#operator-overload-overrides), or the tip is unvouched while not `degraded` |
| `provisioning` | not booted yet: an index-snapshot bootstrap, or plain startup                            |
| `degraded`     | the tip is unvouched, but synced to it while fresh (p2p peers carried it)                |
| `syncing`      | the served tip is not the best yet, or a validator's mempool is off and none is on (zebrad's, for minutes after a catch-up) |
| `serving`      | otherwise                                                                                |

- **Unvouched** = the best block runs more than one past the last block a trusted validator
  vouched for, or every trusted validator is down under a stale best. One block past is the race
  a validator's next poll closes. **Fresh** = the best block's time within 24 target spacings of
  the clock.
- `/readyz` answers `200` only for `serving` and `degraded`. A degraded node keeps serving so a
  load balancer keeps routing to it while every trusted validator is down; alert on the state,
  not on `/readyz`. Requests are never refused for `syncing`.
- `detail` (`null` = nothing measured yet):
  - `provisioning`: `{phase, bytes, total_bytes, bytes_per_sec}`, the snapshot's download,
    verify and unpack; `null` on plain startup.
  - `syncing`: `{height, target, blocks_per_sec}`: the served height, the verified best (or a
    catching-up validator's estimate when higher), and the rate over the last ~60 s.
  - `serving`: `{start, mbps}`: gRPC egress over the last closed UTC 10-minute window (`start` in
    unix seconds, 3 significant figures, under 0.1 published as 0).
  - `degraded`, `down`, `held`: `{reason, message}`, `reason` one of `no_validator_reachable`,
    `tip_unvouched`, `runtime_stalled`, `overloaded`, `goaway_pending`, `goaway_sent`.

### Endpoints

| Listener | Path       | Answers                                                         |
| -------- | ---------- | --------------------------------------------------------------- |
| gRPC     | `/readyz`  | `{"state", "detail"}`; `200` or `503`                           |
| gRPC     | `/status`  | the public projection below, always `200`                       |
| admin    | `/readyz`  | as on the gRPC listener                                         |
| admin    | `/livez`   | `503` once the serving runtime misses its heartbeat for 30 s    |
| admin    | `/metrics` | Prometheus exposition                                           |

The gRPC listener answers both paths over HTTPS/1.1 or h2 for load-balancer monitors, from
process start (before an index-snapshot bootstrap or boot), so they answer `provisioning` or
`down` from the first second; until boot finishes every gRPC method answers `UNAVAILABLE`.
`/status` is safe for the open internet: it carries no validator address, peer, disk, mempool or
tailnet name.

```json
{"state": "serving", "detail": {"start": 1790000400, "mbps": 12.5},
 "chain": {"network": "mainnet", "tip": {"height": 3515406, "hash": "0000…"}, "served": 3515400,
           "validators": {"trusted": 2, "answering": 1}, "alarms": []},
 "build": {"version": "0.11.0-alpha.4"},
 "indexes": {"compact_block": ["GetBlock", "GetBlockRange", "GetBlockRangeNullifiers"], "utxo_set": []}}
```

- `tip` = the verified best block, `served` = the height RPCs answer at (`null` before boot, as
  is `validators`). `answering` counts trusted validators live or catching up.
- `alarms`: one sentence per raised alarm ([architecture §11](./architecture.md#11-telemetry)).
- `indexes` maps each configured index to the RPCs it gates. An RPC answers when the node is
  ready and it names no index or its index is listed.

### Metrics

Every metric carries its HELP text. Every histogram is a Prometheus **native** histogram, carried
only by the protobuf exposition: `/metrics` answers protobuf when the scrape's `Accept` offers
`application/vnd.google.protobuf` (Prometheus or Alloy with `scrape_native_histograms`), else the
text format, which has every counter and gauge but no histogram. zebra-network's own series are
cut to a summary with no peer identity. Filesystem free space and block-device I/O are per host:
read them from the node exporter or cAdvisor.

## Operator hold

`zainod hold` takes a node out of rotation until `zainod resume`, across restarts, deploys and
reboots. It is the only drain: a shutdown signal closes the gRPC listener at once, so a planned
stop is `zainod hold`, then `systemctl stop`.

- `hold` writes the marker `<[serve] state_dir>/held`; `resume` removes it. There is no network
  endpoint: holding a node requires write access to its state directory.
- The running zainod checks the marker every second, from process start. Held, it reports `held`
  above every other state (stopping included), `goaway_pending` and then `goaway_sent` once
  `[serve] hold_goaway_delay_secs` has passed. At that point every gRPC connection, and each one
  accepted while held, gets an HTTP/2 GOAWAY, its in-flight streams `max_connection_age_grace`.
  Set the delay to at least the time your load balancer takes to mark a server down, plus its
  DNS TTL.
- A held process keeps running and answering its health paths and metrics. On resume, a node
  that fell behind reports `syncing` first.
- A stopped or killed zainod is never reported `held`, only absent, so a dashboard can tell a
  planned removal from a crash.
- `hold` blocks until the running zainod reports `goaway_sent` (with `[metrics] listen_address`
  set) or for the longest it can take to send its GOAWAYs, then prints `safe to stop`. Both
  commands print the state on stdout, what changed on stderr, and are idempotent.

## Operator overload overrides

`/readyz` reports `down` with reason `overloaded` while a node is persistently short of capacity
([causes and `[grpc.overload]`](../packages/zaino-grpc/usage.md#overload)), so a load balancer
moves traffic off it. `zainod overload off | on | set <key> <value> | unset <key>` overrides that
with markers in `<[serve] state_dir>/overload/`, which survive restarts like `held`; the running
zainod applies them within a second and logs a WARN naming them.

## systemd

Run under `Type=notify` and zainod reports its own startup. Without `NOTIFY_SOCKET` (any other
service manager, a container, a shell) nothing is sent; it must be a filesystem path, not an
abstract `@` socket.

```ini
[Service]
Type=notify
ExecStart=/usr/bin/zainod start --config /etc/zaino/zainod.toml
Restart=always
# Budget for a start that is not moving (opening indexes, reaching the validator)
TimeoutStartSec=15min
```

- `READY=1` is sent the first time `/readyz` passes, or once the node is held. `systemctl start`
  and `restart` return only once zainod serves at the tip, so a deploy that cannot catch up
  shows as a failed unit.
- Until then each 10 s check that saw progress (snapshot bytes, the fetch, served or an index
  height) extends the start by 5 minutes, so `TimeoutStartSec` bounds only a stalled start. Units
  ordered after zainod wait for it; `systemctl start --no-block` returns at once.
- `TimeoutStopSec` must cover `[grpc] shutdown_grace_secs` plus each index's final commit.

## Stopping and restarts

Boot fails with exit code 1 if a `[[trusted_validators]]` entry cannot make a client, the gRPC
address cannot be bound (before any index-snapshot download) or an index cannot open. On mainnet
and testnet no validator needs to answer for boot to finish; regtest reads its upgrade schedule
from the first validator that answers.

SIGINT or SIGTERM closes the gRPC listener at once, sends every open connection a GOAWAY, waits
up to `[grpc] shutdown_grace_secs` for in-flight streams, lets each index commit what it holds,
and exits 0, during boot too. Behind a load balancer, run [`zainod hold`](#operator-hold) first.

zainod never restarts in-process and stops on any fault it cannot vouch past
([failure policy](../packages/zainod/usage.md#failure-policy)), so run it under a service manager
that restarts it with backoff. Opening an index checks each file's length, tail page and `.crc`
file, never the data pages, so a restarted zainod serves again within seconds, once the NFS has
fetched every block above the durable tips again (logged as `Fetching non-finalized blocks`).

## `zainod verify`

`zainod verify -c /etc/zaino/zainod.toml` reads every file each **enabled** index's `MANIFEST`
seals against its page checksums
([zaino-persistence: Verifying offline](../packages/zaino-persistence/usage.md#verifying-offline)).
It is safe beside a live daemon: plain reads, no mmap, no lock, nothing written. Bytes past a
file's seal (a live writer's next batch) count as `orphaned_bytes`, never as corruption.

The report is JSON on stdout, a summary per index on stderr; exit codes are in `--help`. Per
index: `heights` = durable tip + 1, one entry per sealed file with `lost`, `bad_sums` (the `.crc`
file no longer matches its manifest digest) and `bad_pages` (4 KiB pages whose CRC disagrees);
`null` = disabled. A corrupt or lost file means delete that index's directory and resync it.

## Network exposure

zainod listens on two interfaces:

- **gRPC** (`[serve] grpc_listen_address`): plaintext HTTP/2 unless `[serve.tls]` names a
  certificate pair. Expose it beyond a trusted network only with `[serve.tls]` or behind a
  TLS-terminating proxy. Its `/readyz` and `/status` are safe for the open internet.
- **Admin** (`[metrics] listen_address`, off by default): no authentication or encryption, and
  the metrics publish the chain tip, traffic and validator addresses. zainod warns at startup
  when it binds a non-private address. Keep it on loopback or a network only the scraper reaches.

Outbound, each validator link is plain HTTP JSON-RPC.

```toml
[serve.tls]
cert_path = "/etc/zaino/fullchain.pem"   # chain, leaf first
key_path = "/etc/zaino/key.pem"          # PKCS#8, PKCS#1 or SEC1
```

A bad pair fails startup. A renewed pair is picked up within a minute with no restart, so an
ACME client renewing in place needs no hook; one that fails to load keeps the previous pair
serving.

The `[grpc]` caps refuse rather than queue, except the read lanes; each is described in the
example config and in [zaino-grpc: Serve stack](../packages/zaino-grpc/usage.md#serve-stack).

### Stream time limits

No stream has a cap on its total length, so a full sync streams for as long as it keeps
receiving. Two `[grpc]` bounds catch a stream that makes no progress: `unread_timeout_secs` (its
client stopped reading: the connection closes) and `silent_timeout_secs` (nothing to send: the
stream ends `UNAVAILABLE`; `GetMempoolStream` exempt).

### Behind a proxy

Behind a proxy every connection comes from the proxy's address, so `max_connections_per_ip`
would cap the whole server rather than each client. List the proxy in `trusted_proxies` and have
it send a [PROXY protocol](https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt) header
(v1 or v2) naming the client:

```toml
[grpc]
trusted_proxies = ["10.0.0.5/32"]
```

A connection from a trusted address MUST open with the header, or it is closed; any other
address is served without one. With nginx, terminate TLS in the `stream` module and pass the h2c
through with `proxy_protocol on;` (the `http` module's `grpc_pass` cannot send the header):

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

With HAProxy, use `server zaino 127.0.0.1:8137 send-proxy-v2` in a `mode tcp` backend. A proxy
that cannot send the header needs `trusted_proxies` empty and `max_connections_per_ip` raised to
`max_connections`.

### Open files

Every connection is a file descriptor. At boot zainod raises its soft `RLIMIT_NOFILE` to the
hard limit, then refuses to start if `max_connections` plus 1024 descriptors for index files and
validators does not fit. Raise the hard limit (systemd `LimitNOFILE=`, docker
`--ulimit nofile=`) or lower `max_connections`.

## Logging

logfmt to stdout, filtered by `RUST_LOG`: [what to watch](../packages/zainod/usage.md#logging).
