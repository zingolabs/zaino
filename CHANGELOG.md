# Changelog

Every Zaino crate ships at one shared version, so each section below covers the whole
workspace. [Knope](https://knope.tech) writes each section at release from the `.changeset/`
files merged since the last one ([RELEASING.md](./RELEASING.md)). Under 0.x, a breaking
change bumps the minor version and everything else bumps the patch version.

Releases before 0.11.0 were versioned per crate. Their crate-level history is in each
crate's `CHANGELOG.md` as of tag `zainod-0.10.2` (removed since).

## 0.11.0-alpha.6 (2026-11-04)

### Breaking Changes

#### `zainod`: rebuilt as a lightwalletd-compatible indexer over trusted validators

zainod 0.11 is a new daemon, not an upgrade of 0.10. It serves the lightwalletd `CompactTxStreamer`
gRPC from its own indexes, and reads the chain from one or more trusted Zebra validators.

- **Chain source:** each `[[trusted_validators]]` entry's JSON-RPC (optionally its indexer gRPC).
  zainod trusts what they hold: every block is checked against its header (merkle root, coinbase
  height) and linked to its parent, and the tip follows the most-work header any trusted validator
  serves.
- **Indexes:** compact blocks (each `CompactTx.fee` filled), block hash → height, commitment-tree
  state (Sapling, Orchard, Ironwood) and transparent addresses, each in its own directory. Blocks
  above the indexes' durable tips are held in memory and served, so reorgs never touch disk.
- **Serving:** every request reads one consistent snapshot of the chain and the indexes. gRPC is
  served only while a trusted validator holds the tip (or, with `[p2p]` on, while peers carry a
  fresh tip).
- **Gone:** the JSON-RPC server, the `state` / `direct` backend (Zebra `ReadStateService`,
  `zebra_db_path`), the LMDB finalised-state database and its migrations, the ephemeral
  finalised-state mode and the block cache.

**Migration**

- Rewrite the config (see the config entry) and delete the old `storage.database` directory: it
  is not read.
- The first start builds every enabled index from the trusted validators, or downloads them with
  `[snapshot]`, before `/readyz` passes. Plan for the initial sync.
- Run zainod under a service manager that restarts it: any panic aborts the process and the first
  long-running task to stop ends it (no in-process restart).

#### `zainod`: new config schema; unknown keys are refused

The config file is replaced. A key zainod does not know fails the load, so a 0.10 file does not
start 0.11. Start from `zainod generate-config` (every key at its default) or
`docs/example_configs/zainod.toml`.

Top level: `network`, `validator_client`, `[metrics]`, `[[trusted_validators]]`, `[serve]`
(+ `[serve.tls]`), `[grpc]`, `[submission]`, `[p2p]`, `[sync]` (+ `[sync.lsm_store]`),
`[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]`,
`[index.transparent_address]` and `[snapshot]`.

| 0.10 key | 0.11 key |
| --- | --- |
| `network = "Mainnet" \| "Testnet" \| "PubTestnet" \| "Regtest"` | `network = "mainnet" \| "testnet" \| "regtest"` (default `mainnet`) |
| `[validator_settings]` | one `[[trusted_validators]]` entry (add more for redundancy) |
| `validator_jsonrpc_listen_address` | `jsonrpc_address` (default `127.0.0.1:8232`) |
| `validator_cookie_path` | `cookie_path` |
| `validator_user` / `validator_password` | `user` / `password` |
| `validator_grpc_listen_address` | none (`indexer_address` names zebrad's indexer gRPC) |
| `grpc_settings.listen_address` | `serve.grpc_listen_address` |
| `[grpc_settings.tls]` `cert_path`, `key_path` | `[serve.tls]` `cert_path`, `key_path` |
| `metrics_endpoint` | `metrics.listen_address` |
| `ZAINO_*` environment overrides | `ZAINO_CONFIG_*` (`__` nests: `ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS`) |

Removed with no replacement: `backend`, `zebra_db_path`, `ephemeral_finalised_state`,
`json_server_settings`, `service`, `storage` (`cache`, `database`), `mempool` and
`donation_address`.

Behaviour of the moved keys:

- `network` is declared, never read off the validator (Zebra on regtest reports `test`).
- `user` and `password` are set together or not at all. With neither, no basic auth is sent
  (0.10 sent `xxxxxx:xxxxxx` by default).
- Checked at startup: at least one and at most 64 `[[trusted_validators]]`, no address listed
  twice, at least one index enabled, and `sync.finalised_depth` ≥ 1000 off regtest.

New keys are described in the entries for the features they configure.

#### `zainod`: one directory per index, on a new storage engine

The 0.10 finalised-state database is not read or migrated. Each index lives in its own
directory, set by `[index.<name>] path`:

| Index | Default directory | Serves |
| --- | --- | --- |
| `compact_block` | `<cache dir>/zaino/indexes/compact_block` | `GetBlock`, `GetBlockRange` |
| `utxo_set` | beside `compact_block` | each `CompactTx.fee` (on and off with `compact_block`) |
| `block_hash` | `<cache dir>/zaino/indexes/block_hash` | requests by block hash |
| `tree_state` | `<cache dir>/zaino/indexes/tree_state` | `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` |
| `transparent_address` | `<cache dir>/zaino/indexes/transparent_address` | `GetAddressUtxos(Stream)`, `GetTaddressBalance(Stream)`, `GetTaddressTransactions` |

In the container image `<cache dir>/zaino` is `/var/lib/zaino`.

- Every index commits atomically through a manifest, and checks a CRC on every 4 KiB page when it
  first reads it. A crash or power loss leaves the last commit.
- An index directory opened for another network, another index or another format is refused.
- Blocks reach disk once they are `sync.finalised_depth` deep. Blocks above that are held in
  memory, so a restart refetches them.

**Migration:** delete the old `storage.database` directory. A first start syncs each enabled
index from the trusted validators, or fills empty ones from `[snapshot]`.

#### `zainod`: Zaino serves no JSON-RPC

The JSON-RPC server, its cookie authentication and port 8237 are gone, with every method it
served (`getblock`, `getrawtransaction`, `z_gettreestate`, `getaddressdeltas`, `getblockdeltas`,
…).

**Migration:** send node RPC to the validator's own JSON-RPC. Wallets keep using the
lightwalletd gRPC.

#### `zainod`: gRPC methods removed, and answers that changed

Removed (each answers `UNIMPLEMENTED`):

- `zaino.index.v1.IndexedTipService` (added in 0.10.1): poll `GetLatestBlock` instead.
- `GetBlockNullifiers`: use `GetBlock`, or `GetBlockRange` with `poolTypes`.
- `Ping`.

Still served, as deprecated aliases that will be removed: `GetBlockRangeNullifiers`
(= `GetBlockRange`, nullifiers only) and `GetTaddressTxids` (= `GetTaddressTransactions`).

Answers that changed:

- `GetLightdInfo` reads no validator: `blockHeight` = the served tip, `estimatedHeight` = the
  best verified header, consensus branch and next upgrade from the upgrade schedule at the served
  tip. `lightwalletProtocolVersion` = `v0.6.0`. `donationAddress` is empty. Before anything is
  served it answers `UNAVAILABLE`.
- A method whose index is disabled answers `UNIMPLEMENTED`. While zainod starts up, every method
  answers `UNAVAILABLE`.
- Requests by height (`GetBlock`, `GetBlockRange`, `GetTreeState`) answer up to their own index's
  durable tip, which during a sync can be above the served tip. `GetLatestBlock`,
  `GetLatestTreeState`, `GetSubtreeRoots` and the address methods answer at the served tip.
- `GetBlockRange` with `start > end` streams in decreasing height order. A range reaching past
  the tip streams every block up to it, then ends `OUT_OF_RANGE`, as lightwalletd does.
- `poolTypes`: an empty list = Sapling, Orchard and Ironwood (transparent excluded). An unknown
  value is `INVALID_ARGUMENT`. `GetMempoolTx` filters by `poolTypes` as `GetBlockRange` does.
- `GetTreeState` below Sapling activation answers that height with every tree `""`.
- `GetSubtreeRoots` with an unknown `shieldedProtocol` is `UNIMPLEMENTED`.
- `GetTransaction` asks the trusted validators for the bytes. It is `NOT_FOUND` only when every
  validator asked says absent. A transaction not on the best chain reports height `0`.
- One address request walks at most `[serve] max_address_rows` (100,000) receives across its
  addresses, and is `RESOURCE_EXHAUSTED` past that, never a partial answer.
- The client's `grpc-timeout` is its deadline (`DEADLINE_EXCEEDED`). Streams have no total-duration
  cap.
- Request bodies are capped at 64 KiB, or 2 MB + 1 KiB for `SendTransaction`
  (`RESOURCE_EXHAUSTED`).
- `GetMempoolTx` and `GetMempoolStream` list a transaction once any trusted validator lists it
  (or the `[p2p]` peer quorum admits it).
- `GetMempoolStream` ends when the served tip moves, so the `GetLatestBlock` that follows serves
  the new block. It never replays transactions the served tip already mined.

#### `zainod`: new metric families; every 0.10 `zaino_*` / `zainod_*` metric is gone

Metrics are always compiled in (the `prometheus` feature is gone) and served on the admin
listener's `/metrics`. Dashboards and alerts need rewriting: only the `process_*` families carry
over.

| 0.10 metric | Nearest 0.11 metric |
| --- | --- |
| `zaino_chain_tip_height`, `zaino_sync_target_height` | `zaino_best_tip` |
| `zaino_sync_fetched_height` | `zaino_fetch_height` |
| `zaino_sync_finalized_height` | `zaino_index_finalized_height{index}` |
| `zaino_sync_fetched_*_total` | `zaino_fetch_*_total` |
| `zaino_sync_reorg_total` | `zaino_reorgs_total` |
| `zaino_grpc_request_duration_seconds` | `zaino_grpc_duration_seconds{method}` |
| `zaino_grpc_errors_total` | `zaino_grpc_requests_total{method, code}` |
| `zaino_rpc_outbound_duration_seconds`, `zaino_rpc_outbound_errors_total` | `zaino_validator_rpc_duration_seconds`, `zaino_validator_rpc_failures_total` |
| `zaino_mempool_transactions` | `zaino_chainview_mempool_transactions{state}` |
| `zainod_build_info` | `zaino_build_info` |

Removed with no replacement: `zaino_db_*`, `zaino_migration_*`, `zaino_jsonrpc_*`, the rest of
`zaino_sync_*` and `zaino_mempool_*`, and `zainod_restarts_total`.

0.11 families:

- **Chain and sync:** `zaino_best_tip`, `zaino_fetch_height`, `zaino_fetch_*_total` (blocks,
  transactions and per-pool work), `zaino_reorgs_total`, and per `{index}`:
  `zaino_index_synced`, `zaino_index_finalized_height`, `zaino_index_applied_height`,
  `zaino_index_applied_{blocks,rows}_total`, `zaino_index_{run,write}_seconds`.
- **Validators:** `zaino_validator_rpc_*`, `zaino_traffic_misanswers_total`, and the chain
  view's `zaino_chainview_*` (tip holders, agreement, validator state and release, push streams,
  peers, finality pause, mempool, submissions).
- **State:** `zaino_state{state}` and `zaino_state_reason{reason}`, one-hot over `/readyz`'s
  state and detail.
- **gRPC serving:** `zaino_grpc_*` per `{method}` (requests, one-reply and first-message
  latency, bytes sent), connection, admission, subscription and adaptive-limit gauges and
  counters, and `zaino_grpc_overload_latched{cause}`.
- **Peers** (`[p2p]` on): `zaino_peers_request_duration_seconds{request}`,
  `zaino_peers_request_failures_total{request, failure}`, and zebra-network's `zcash_net_peers`,
  `pool_num_{ready,unready}`, `zcash_net_{in,out}_messages{command}`,
  `zcash_net_{in,out}_bytes_total`, `zcash_net_connection_outcomes_total`.
- **Storage:** `zaino_store_commit_{seconds,bytes_total}{index}`, `zaino_lsm_*{index}`,
  `zaino_disk_{committed,cached}_bytes{index}`, `zaino_disk_bytes{index}` (sampled every 30 s).
- **Process:** `process_*` (CPU, resident memory, file descriptors, threads).

Every histogram is a Prometheus native histogram, so `/metrics` serves protobuf when the scraper
offers `application/vnd.google.protobuf` (a text scrape carries no histogram). Prometheus: set
`scrape_native_histograms: true` on the zainod job. Alloy: `scrape_native_histograms = true` on
`prometheus.scrape` and `send_native_histograms = true` on each `prometheus.remote_write`.
Queries drop `_bucket` and `le`:
`histogram_quantile(0.95, sum by (method) (rate(zaino_grpc_duration_seconds[5m])))`.

#### `zainod`: command line and log format

Commands: `start`, `generate-config [-o FILE]`, `verify`, `hold`, `resume` and
`overload on|off|set|unset`. `-c, --config FILE`
is accepted before or after any command (default `$XDG_CONFIG_HOME/zaino/zainod.toml`), and
`generate-config` without `-o` writes to that path.

Logs are logfmt on stdout, one line per event (`ts=… level=… target=… message=… key=value …
component=…`), coloured only on a terminal. `RUST_LOG` filters (unset = `zaino=info,zainod=info`);
a malformed one fails startup.

**Migration:** `ZAINOLOG_FORMAT`, `ZAINOLOG_COLOR` and `ZAINOLOG_LOCATION` are gone (JSON output
included): unset them and parse logfmt.

#### Releases: one workspace version, the `zainod` image only, nothing on crates.io

- Every crate ships at one version, released as tag `vX.Y.Z` (was per-crate `zainod-X.Y.Z`), with
  one `CHANGELOG.md`.
- Docker Hub: `zainod:X.Y.Z` and `zainod:latest` only. The `zaino` repository and the `-no-tls`
  image variant are no longer published.
- No crate is published to crates.io.
- `zainod` has no cargo features: `prometheus`, `no_tls_use_unencrypted_traffic`,
  `no_tls_with_prometheus`, `allow_unencrypted_public_json_rpc_bind`, `experimental_features`,
  `transparent_address_history_experimental`, `test_dependencies` and `fast-test-seam` are gone.
  Metrics, the admin listener, the transparent-address index and the `[snapshot]` bootstrap are
  always built. Building needs `protoc` (on `PATH` or named by `PROTOC`).

Container image ([docs/docker.md](../docs/docker.md)):

- Alpine base, static musl `zainod` with jemalloc as its allocator (was Debian bookworm-slim).
- Runs as `zaino` (uid 1000). Entrypoint `zainod`, default command `start`; `entrypoint.sh` is gone.
- Config at `/etc/zaino/zainod.toml` (was `/app/config`), data under `/var/lib/zaino` (was
  `/app/data`): `indexes/`, `state/` (the hold marker), `peers/`. TLS files by convention in
  `/etc/zaino/tls/`.
- gRPC listens on `0.0.0.0:8137` by default in the image (`ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS`).
  `ZAINO_STORAGE__DATABASE__PATH` and `ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR` are no longer read.
- Exposes only the gRPC port, 8137 (no JSON-RPC port).
- Built-in `HEALTHCHECK`: the public `/readyz` on 8137, plaintext or `[serve.tls]`.
- Ships `aria2c` for the `[snapshot]` bootstrap. The `CARGO_FEATURES` build argument is gone.

#### Library crates: new workspace layout

The libraries are rebuilt around the daemon and are not published. `zaino-primitives`,
`zaino-proto`, `zaino-source` and `zaino-persistence` keep their names but not their APIs.
`zainod` is a binary only (`zainodlib` is gone).

| Crate | Role |
| --- | --- |
| `zaino-primitives` | Domain types on librustzcash's (`BlockHeight`, `BlockHash`, `TxId`, transparent `OutPoint` and `Script`, network-upgrade schedules) |
| `zaino-proto` | lightwallet-protocol v0.6.0 and zebrad indexer bindings, generated at build (`protoc`); client half only for `CompactTxStreamer` |
| `zaino-source` | `ChainDataSource`, one port per validator: JSON-RPC client and zebrad indexer gRPC client |
| `zaino-traffic` | `TrafficBalancer`: every request to trusted validators and peers |
| `zaino-peers` | Zcash p2p layer (zebra-network) |
| `zaino-header-chain` | Most-work header tree above a trusted anchor |
| `zaino-chainview` | Best chain, finality, mempool and transaction submission over trusted validators and peers |
| `zaino-persistence` | Storage port every index stores through, and `DiskEngine` |
| `zaino-sync` | Finalized block stream, index writers and their commit cadence |
| `zaino-nfs` | Non-finalized state: blocks above the durable tips, folded in memory |
| `zaino-index-compact-block`, `zaino-index-tree-state`, `zaino-index-transparent-address`, `zaino-index-utxo-set`, `zaino-internal-block-hash-to-height` | One crate per index |
| `zaino-grpc-snapshot` | `GrpcSnapshot`: one atomic publication of what gRPC serves |
| `zaino-grpc` | lightwalletd `CompactTxStreamer` server, health endpoints, admission and limits |

Removed: `zaino-address`, `zaino-async`, `zaino-chain`, `zaino-chain-head`,
`zaino-chain-head-service`, `zaino-chain-store`, `zaino-chain-store-zainodb`, `zaino-common`,
`zaino-component`, `zaino-consensus`, `zaino-convert-zebra`, `zaino-encoding`, `zaino-logging`,
`zaino-mempool`, `zaino-mempool-service`, `zaino-rpc`, `zaino-serve`, `zaino-source-macros`,
`zaino-source-zebra`, `zaino-source-zebra-readstate`, `zaino-source-zebra-rpc`, `zaino-state` and
`zaino-status`.

Each crate's `usage.md` (`README.md` for `zaino-proto`) documents its API, and
`docs/architecture.md` how the crates fit together.

### Features

#### `zainod`: several trusted validators, one traffic balancer

`[[trusted_validators]]` takes 1 to 64 entries, all equal: no vote, and more never weakens the
view. The served tip is the most-work header chain any of them serves, served while one holds it.
Two or more survive one going down.

Per entry:

| Key | Default | Meaning |
| --- | --- | --- |
| `jsonrpc_address` | `127.0.0.1:8232` | Zebra JSON-RPC |
| `cookie_path` / `user` + `password` | none | authentication |
| `connect_timeout_secs` | 2 | |
| `read_timeout_secs` | 30 | silence before a request fails, not total duration |
| `max_connections` | 32 | link budget, at least 5 (one for the poll, one reserved per lane + one shared) |
| `max_response_mib` | 32 | largest JSON-RPC reply, 4 ..= 32; batches split to fit |
| `priority` | 0 | lower tiers asked first; a higher tier takes overflow and hedges only |
| `indexer_address` | none | zebrad's indexer gRPC: tip and mempool push streams wake polling at once |
| `[trusted_validators.indexer_tls]` | plaintext | mutual TLS to `indexer_address`: `ca_path`, `cert_path`, `key_path`, optional `server_name` |

- One balancer schedules every request (polls, headers, blocks, lookups, submissions) in three
  lanes per validator (control, interactive, bulk), so a bulk sync never starves the tip.
- A validator serving a block or header that fails its check is benched for 60 s, doubling up to
  an hour (`Misanswer, member benched`, `zaino_traffic_misanswers_total`).
- A validator down at boot or later is retried with backoff and never stops zainod. On mainnet
  and testnet zainod boots without one answering (the upgrade schedule is compiled in); regtest
  reads it from the first validator that answers.
- `validator_client = "grpc"` (default `legacy_rpc`) fetches blocks over each validator's
  `indexer_address` as raw bytes instead of hex inside JSON, everything else over JSON-RPC. Every
  entry then needs `indexer_address`, and a zebrad serving the indexer's `GetBlockRange`
  (Zaino's zebra fork).

#### `zainod`: `[p2p]` peers keep the tip and mempool serving through a validator outage

With `[p2p] enabled = true` (default off), zainod joins the Zcash p2p network through
zebra-network. It serves no peer (listener on loopback, no services advertised).

- **Headers and blocks:** peers' headers extend the tip once they pass every header rule against
  zainod's own chain (proof of work, difficulty, time, linkage). Blocks are fetched by hash and
  checked against their header.
- **Finality:** moves on peers' word once outbound peers in more than 5 netgroups carry the chain
  and its difficulty has not collapsed. Otherwise it waits for a trusted validator.
- **Mempool:** each outbound peer's mempool is polled every 5 s. A transaction more than 2/3 of
  them list, from more than 5 netgroups, is served once it passes Zaino's consensus checks against
  the served UTXO set (needs `index.compact_block`).
- **Submission:** `SendTransaction` relays through the peer set (see the submission entry).
- With every trusted validator down and a best block less than 24 target spacings old (30 minutes
  at 75 s, 10 from NU7), zainod keeps serving as `degraded` (`/readyz` `200`). A stale tip is
  `down`.

| Key | Default | |
| --- | --- | --- |
| `enabled` | `false` | |
| `peer_target` | 8 | outbound peers, at least 3 |
| `initial_peers` | `[]` | empty = the network's DNS seeders; regtest must list peers |
| `cache_dir` | `<cache dir>/zaino/peers` | peer address cache |

#### `zainod`: `SendTransaction` checks admission first, then relays until the transaction spreads

- zainod first asks one trusted validator holding the tip for `checkadmission` (Zaino's zebra
  fork: `sendrawtransaction`'s checks, fee policy and mempool conflicts, nothing admitted or
  relayed). A refusal reaches the wallet at once and nothing is relayed. An admissible
  transaction is answered `OK` at once.
- Validators without `checkadmission` (stock zebrad) answer method-not-found. They stay healthy
  and the wallet is answered at the first relay's acceptance.
- Each attempt relays through one random trusted validator, or with `[p2p]` on through a third
  of the ready peers. The transaction is relayed again when it is not seen spreading within
  `[submission] propagation_threshold_secs` (15), up to `max_attempts` (4) relays.
- Rejections answer as lightwalletd does: gRPC `OK` carrying the validator's own JSON-RPC error
  code and message (`-22` for unparseable bytes, `-25` for an expired transaction or the wrong
  consensus branch). zebrad's "already in mempool" and "already queued" are rejections, not
  `UNAVAILABLE`.
- A sent transaction is listed by `GetMempoolTx` and `GetMempoolStream` before it propagates.

#### `zainod`: per-index `enabled` and `path`; `[sync]` budgets

- `[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]` and
  `[index.transparent_address]` each take `enabled` (default `true`) and `path`. A disabled
  index is never built and its methods answer `UNIMPLEMENTED`. At least one must stay enabled.
- `[sync]`, shared by every index:

| Key | Default | Meaning |
| --- | --- | --- |
| `finalised_depth` | 1000 | blocks below the tip kept reorg-able in memory; at least 1000 off regtest |
| `concurrency` | 32 | block fetches in flight ahead of the next one needed |
| `queue_mib` | 256 | finalized blocks one index may trail the fetch by before the fetch waits |

- `[sync.lsm_store]` tunes the storage engine's background merges: `fanout` (16, at least 2),
  `merge_slots` (4, merges doing disk I/O at once across every index) and `merge_mib_per_sec`
  (200, read + write bandwidth those merges share; commits are never paced). A `fanout` an
  enabled index cannot hold is refused at startup.

#### `zainod`: `[snapshot]` fills empty indexes from a published IndexSnapshot

```toml
[snapshot]
manifest = "https://…/manifest.json"
connections = 8   # total across every archive, at most 16
```

- Before boot, each enabled index whose directory is missing or empty is downloaded from its
  archive in the manifest. An index holding data is never touched.
- Free space is checked per filesystem before any download. `aria2c` (on `PATH`; shipped in the
  image) downloads and checks each archive's sha256, then it is unpacked beside its index and
  renamed into place.
- A restart resumes each archive at the step it stopped in. SIGTERM / SIGINT during the download
  stops `aria2c` and exits 0.
- Progress shows as state `provisioning` with its bytes as `detail` on `/readyz` and `/status`.

#### `zainod`: readiness and status endpoints

Admin listener (`[metrics] listen_address`, unset = off), on its own thread and runtime:

- `/metrics`: Prometheus exposition.
- `/livez`: fails once the serving runtime has missed its heartbeat for 30 s.
- `/readyz`: `{"state", "detail"}`, `200` only when `serving` or `degraded`.

Public endpoints on the gRPC listener (same port and TLS, HTTP/1.1 or h2), safe to expose:

- `GET /readyz`: same body as the admin `/readyz`.
- `GET /status`: `{state, detail, chain, build, indexes}`. `chain` = network, best tip, served
  height, trusted / answering validator counts, alarms; `build` = version; `indexes` = each
  configured index → the RPCs it gates. No addresses, peers, disk or mempool.

`state` is one of, each with its own `detail`:

| State          | Meaning                                                                   | `detail`                                              |
| -------------- | ------------------------------------------------------------------------- | ----------------------------------------------------- |
| `provisioning` | starting: index-snapshot download, or plain startup                       | snapshot `{phase, bytes, total_bytes, bytes_per_sec}` |
| `syncing`      | indexes or the served tip behind the network's tip                        | `{height, target, blocks_per_sec}`                    |
| `serving`      | a trusted validator holds the served tip                                  | egress, last closed 10-minute window `{start, mbps}`  |
| `degraded`     | `[p2p]` peers carry a fresh tip that no trusted validator vouches for     | `{reason, message}`                                   |
| `held`         | operator hold (`zainod hold`)                                             | `{reason, message}`                                   |
| `down`         | no trusted validator reachable, tip not held, runtime stalled, overloaded | `{reason, message}`                                   |

A node persistently short of capacity reports `down` / `overloaded` (`Overloaded: <cause>`:
CPU or IO pressure, refusing work) until it recovers. Under systemd `Type=notify`, zainod sends `READY=1` the first time
`/readyz` passes (or the node is held), and extends the start timeout while a sync or snapshot
download makes progress.

#### `zainod`: `[grpc]` serving limits and load shedding

Every cap refuses rather than queues (`UNAVAILABLE` with a retry hint), except the read lanes,
which wait. Defaults shown:

| Key | Default | Bounds |
| --- | --- | --- |
| `max_connections` | 4096 | open connections (+ 1024 must fit the open-file limit, or boot fails) |
| `max_connections_per_ip` | 32 | per client: the peer, or the address a trusted proxy names |
| `max_streams_per_connection` | 8 | h2 streams on one connection |
| `max_work_streams` | 2048 | streams of every method but `GetMempoolStream` |
| `adaptive_work_streams` | `true` | admitted work follows measured latency, `max_work_streams` the ceiling |
| `max_subscriptions` | 4096 | `GetMempoolStream` subscriptions |
| `max_point_reads` / `max_range_reads` / `max_scan_reads` | 64 / 32 / 4 | index reads in flight per lane |
| `unread_timeout_secs` | 300 | data the client leaves unread → connection closed |
| `silent_timeout_secs` | 300 | nothing to send → stream ends `UNAVAILABLE` (`GetMempoolStream` exempt) |
| `max_connection_age_secs` | 1800 | GOAWAY so clients re-resolve DNS (±10%; 0 = never) |
| `max_connection_age_grace_secs` | 600 | in-flight streams of a GOAWAY'd connection |
| `max_connection_idle_secs` | 1800 | GOAWAY a connection with no open stream (0 = never) |
| `trusted_proxies` | `[]` | proxies that send a PROXY protocol v1/v2 header |
| `shutdown_grace_secs` | 10 | open connections' time to finish after SIGTERM / SIGINT |

- `[grpc.overload]` latches `/readyz` `down` / `Overloaded: <cause>` on `CPU pressure` or
  `IO pressure` (PSI, 25%), or `refusing work`. `zainod overload on|off|set|unset` overrides the detector across
  restarts, as `zainod hold` does.
- `[serve.tls]` certificates are re-read when they change: renewal needs no restart.
- Streamed answers pack ready messages into full 16 KiB HTTP/2 DATA frames, so h2 0.4.16+ clients
  that read late no longer hit `GOAWAY too_many_data_frames`.

#### `zainod`: `zainod hold` / `zainod resume` take a node out of rotation

- `zainod hold` writes a marker in `[serve] state_dir` (default `<data dir>/zaino/state`) that
  survives restarts, deploys and reboots. The running zainod reads it within a second: state
  `held` (`/readyz` `503`), and after `[serve] hold_goaway_delay_secs` (default 0) every gRPC
  connection, open or opened later, is sent GOAWAY. Health and admin requests keep answering.
- `zainod hold` returns once the running zainod reports `GOAWAY sent`, then prints `safe to stop`.
  `zainod resume` lifts the hold. Both are idempotent and exit 0 (1 on a config or state-directory
  error).
- SIGTERM / SIGINT closes the gRPC listener at once and gives open connections
  `[grpc] shutdown_grace_secs`, then exits 0. Behind a load balancer, hold before stopping.
- Under systemd, a node started held completes its start.

#### `zainod verify` checks every index file against its page checksums

`zainod verify [-c FILE]` reads every file each enabled index has made durable and checks every
page's CRC. It is read-only and safe beside a running zainod: it opens nothing for writing,
creates no directory and takes no lock. Bytes a live writer appended past the durable length are
reported as orphaned, not as corruption.

Prints a JSON report on stdout and a summary on stderr. Exits 0 when every page is intact, 1 when
any file is lost or corrupt, 2 when the indexes could not be read.

#### `zainod`: `GetStandardFee` over gRPC

`GetStandardFee` (lightwallet-protocol v0.6.0) answers `StandardFee { feeZat, estimatorVersion }`:
the ZIP 317 marginal fee in zatoshis per logical action for the block after the best tip, with
estimator version 0. zainod computes it from the network and that height, as zebrad's
`getstandardfee` does (5,000 on mainnet below 3,590,000, then 1,000; 1,000 on test networks), so it
answers with no validator reachable. With no header chain yet it answers `UNAVAILABLE`.

#### `zainod`: NU7 on Mainnet

NU7 activates on Mainnet at height 3,543,405 (branch id `0x77190ad9`). zainod's compiled schedule
carries it (`zcash_protocol` 0.11.0), so from that height:

- Headers check against ZIP 218's 25 s target spacing and 102-block averaging window.
- `GetLightdInfo` names NU7 as the next upgrade below the height, then its branch id from it.
- The end-of-service alarm fires within a week of target time of a validator's halt, at each
  height's spacing (75 s, then 25 s).

Trusted validators and p2p peers must run Zebra 7.0.0 or later: zainod's peer layer speaks network
protocol version 170190, and a Zebra 6 validator leaves the consensus chain at the activation height.

#### `zainod`: `zaino_finalized_tip` gauge

`zaino_finalized_tip` is the height of the finalized boundary: the last block the follower hands
the index writers. `zaino_fetch_height` climbs to it during a sync, while `zaino_best_tip` stays
about 1,000 blocks above it.

## 0.4.1 (2026-06-18)
- Bump zaino-proto 0.1.2 → 0.1.3 and zainod 0.4.0 → 0.4.1 to work around
  a yanked 0.1.2 slot on crates.io. No code changes.

## 0.4.0 (2026-06-17)
- NU6.2 network upgrade is now supported: activation-height configuration
  (`zaino-common`) and Zebra RPC response parsing (`zaino-fetch`) recognise
  NU6.2.
- [943] Zallet regtest fixes
- [1065] Move functionality to BlockChainSource: t-address rpcs
- `gettxoutsetinfo` is now served indexer-side. Both `FetchService` and
  `StateService` compute the response from Zaino's own UTXO-set accumulator
  (finalised state + non-finalised state) instead of forwarding to the backing
  validator.

### Added
- `storage.database.sync_write_batch_bytes` config (default 4 GiB) tunes the
  finalised-state bulk-sync / migration write-batch size.
- `zainod` gains an `allow_unencrypted_public_json_rpc_bind` build feature that
  lifts the new private-only JSON-RPC bind restriction for trusted
  private-network deployments (logs a `WARN` on startup when enabled).
- `zaino-state::chain_index::source::BlockchainSource` and
  `zaino-state::chain_index::ChainIndex` now expose transparent-address query
  methods for deltas, balances, txids, and UTXOs.
- `ChainIndex::get_tx_out_set_info` — combines the finalised
  `FinalisedTxOutSetInfoAccumulator` with the non-finalised state to produce
  the full `GetTxOutSetInfoResponse`.
- Optional ("ephemeral") finalised state: `zainod` gains an
  `ephemeral_finalised_state` config option (default `false`) that runs Zaino
  without a persistent finalised-state database, serving finalised reads from
  the backing validator via an ephemeral passthrough.
- `ChainIndex::get_outpoint_spenders` — resolves, for each transparent
  outpoint, the txid that spent it on the best chain (or `None` if unspent),
  with a `ChainScope` selecting finalised-only or full-chain search.
### Changed
- Finalised-state sync and the v1.1.0 -> v1.2.0 migration are substantially
  faster on large/mainnet caches. The txout-set accumulator is built in bulk at
  the tip instead of per block (removing an unbounded fan-out of random reads),
  block validation is off the write path, and the random-keyed `spent` /
  `txid_location` indexes are written in sorted batches — together removing the
  random-fault stall around sandblast height. See the `zaino-state` changelog for
  details; tune the write-batch size with `storage.database.sync_write_batch_bytes`.
- Finalised-state sync and version migrations are now background, non-blocking
  operations: large syncs and migrations run while an ephemeral passthrough
  serves finalised reads, so startup and serving are no longer blocked on
  persistence. Internally the finalised-state facade `ZainoDB` was renamed
  `FinalisedState` and its backing `DbBackend`/`db` module became
  `FinalisedSource`/`finalised_source` (now covering an ephemeral passthrough,
  not only databases). Bumps the finalised DB version to v1.2.1 (metadata-only).
- The `zainod` JSON-RPC server now refuses to bind to public or unspecified
  (`0.0.0.0` / `::`) addresses by default; `check_config` enforces the same
  private/loopback rule already applied to gRPC. The unencrypted JSON-RPC
  interface is intended for loopback or trusted private networks only (Z-02 /
  Zellic #48480).
- `get_address_utxos` now bounds the number of addresses fanned out per request,
  preventing an unbounded multi-address query from amplifying backend load
  (#974).
- Integration tests now use `corez`, with Zcash, Zebra, and Zingo dependencies
  updated to releases and companion branches that no longer depend on the
  yanked `core2` crate.
- Integration tests now follow the companion Zingo corez migration branches and
  use `zcash_client_backend` 0.22, with deprecated nullifier-range client calls
  allowed locally until they are replaced.
- `JsonRpSeeConnector::get_tree_state` now returns a `GetTreestateResponse`
  whose `sapling` and `orchard` fields are optional. In regtest mode, these
  fields may be omitted when the corresponding network upgrade activation
  height is not configured.
### Removed
### Deprecated
### Fixed
- Finalised-state DB v1.2.0 migration no longer appears to hang on large caches.
  A reverse transaction-id index (`txid_location`) makes previous-output
  resolution an O(log n) lookup instead of a full table scan, removing a
  near-quadratic cost in both the migration backfill and the clean-sync write
  path. The v1.1.0 -> v1.2.0 migration is now a re-entrant two-stage backfill
  with progress logging, and caches built by 0.4.0-alpha.1 self-heal on open.
- Nullifiers-only compact blocks (`compact_block_to_nullifiers`) no longer leak
  transparent `vin` / `vout`, restoring lightwalletd compact-block parity
  (#1067).

## 0.3.1 (2026-05-25)

Re-release of 0.3.0 to publish the `zainod` binary's container image under the
new `zainod` Docker Hub repository alongside the legacy `zaino` repository
(#1133, #1134). No functional changes to any crate since 0.3.0.

## 0.3.0 (2026-05-22)

### Added
- Transparent-address queries on the `zaino-state` `ChainIndex` trait —
  `get_address_balance`, `get_address_deltas`, `get_address_txids`,
  `get_address_utxos` (#1065) — plus block lookups (#1000) and subtree-root
  reporting (#853).
- `zaino-state` shared `CommonBackendConfig` payload carrying an
  `indexer_version` field, and a `DonationAddress` type (#1008).
- `zainodlib::config::ZainodConfig` gains an optional `donation_address` field;
  0.2.0 TOML configs continue to load (the field defaults to absent) (#1008).
- `z_validateaddress` JSON-RPC passthrough across `zaino-fetch` and the
  `zaino-serve` `ZcashIndexerRpc` trait, shipped pre-deprecated (#389).
- `zaino-common` `logging` module — the initial structured-logging surface for
  the Zaino crates (#888).
- `zaino-proto` Cargo features `heavy` (default) and `grpc_proxy_server`; build
  wiring moved to `tonic-prost` / `tonic-prost-build` 0.14.

### Changed
- **Breaking** — the `ChainIndex` (`zaino-state`) and `ZcashIndexerRpc`
  (`zaino-serve`) traits gain required methods with no default body, so
  downstream implementers must add them; adding `donation_address` to
  `ZainodConfig` is likewise breaking for struct-literal construction (#1008).
- `LightdInfo.version` now reports the running `zainod` binary version rather
  than the `zaino-state` library version (#1061).

### Fixed
- Restart path no longer crashes when the validator's readiness signal arrives
  before the indexer's status is observed (#962).

## 0.2.0 (2026-03-25)
- [808] Adopt lightclient-protocol v0.4.0

### Added
### Changed
- zaino-proto now references v0.4.0 files
- `zaino_fetch::jsonrpsee::response::ErrorsTimestamp` no longer supports a String
  variant.
### Removed

### Deprecated
- `zaino-fetch::chain:to_compact` in favor of `to_compact_tx` which takes an
  optional height and a `PoolTypeFilter` (see zaino-proto changes)
- `zaino_fetch::FullTransaction::to_compact` deprecated in favor of `to_compact_tx` which includes
  an optional for index to explicitly specify that the transaction is in the mempool and has no
  index and `Vec<PoolType>` to filter pool types according to the transparent data changes of
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Block::to_compact` deprecated in favor of `to_compact_block` allowing callers
  to specify `PoolTypeFilter` to filter pools that are included into the compact block according to
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Transaction::to_compact` deprecated in favor of `to_compact_tx` allowing callers
  to specify `PoolTypFilter` to filter pools that are included into the compact transaction according
  to lightclient-protocol v0.4.0.

---

The lightwallet / `walletrpc` **protocol** changelog (proto-definition version history,
v0.1.0 → v0.4.0) is at `packages/zaino-proto/lightwallet-protocol/CHANGELOG.md`.
