# Zaino

Zaino is a Zcash indexer written in Rust. `zainod` follows the chain from one or more trusted
[Zebra](https://github.com/ZcashFoundation/zebra) validators and, optionally, Zcash p2p peers,
builds its own indexes, and serves light wallets over the lightwalletd-compatible
`CompactTxStreamer` gRPC service.

Validation stays in Zebra and indexing moves to Zaino: the two run as separate processes, on
separate hosts if you like, and a new index needs no change to the validator
([who owns what](./docs/architecture.md#1-who-owns-what)).

[![latest release](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=v*&label=latest%20release)](https://github.com/zingolabs/zaino/tags)

## Quick start

```sh
cargo run --release -p zainod -- generate-config   # then edit; every key: docs/example_configs/zainod.toml
cargo run --release -p zainod -- start
cargo nextest run --workspace                      # unit tests, no validator
```

## Documentation

- [Architecture](./docs/architecture.md): **start here.** How the pieces fit and why.
- [Running zainod](./docs/running.md): configuration, states, hold, systemd, network exposure.
- [RPC API](./docs/rpc_api.md): the methods served and the status codes a client must tell apart.
- [What the clients actually call](./docs/client-requirements.md): the two wallet sync engines.
- [Legacy surface](./docs/legacy-surface.md): every RPC and index the 0.10 stack served, and what
  is not built yet.
- [Docker](./docs/docker.md), [Testing](./docs/testing.md),
  [Updating Zebra](./docs/updating_zebra_crates.md), [Glossary](./CONTEXT.md).
- [CONTRIBUTING.md](./CONTRIBUTING.md), [RELEASING.md](./RELEASING.md),
  [live-test guidelines](./live-tests/CLAUDE.md).

### Crate usage guides

All crates live under `packages/`; each guide covers that crate's API, invariants and tests.

- [`zainod`](./packages/zainod/usage.md): the daemon binary.
- [`zaino-grpc`](./packages/zaino-grpc/usage.md): the gRPC routes and serve stack.
- [`zaino-grpc-snapshot`](./packages/zaino-grpc-snapshot/usage.md): the one published `GrpcSnapshot` of everything served.
- [`zaino-chainview`](./packages/zaino-chainview/usage.md): header sync, the mempool view and submission.
- [`zaino-header-chain`](./packages/zaino-header-chain/usage.md): the Validated Header tree, finality, `validate`.
- [`zaino-nfs`](./packages/zaino-nfs/usage.md): the non-finalized state.
- [`zaino-sync`](./packages/zaino-sync/usage.md): the finalized stream and every index writer's loop.
- [`zaino-persistence`](./packages/zaino-persistence/usage.md): the persistence port, `DiskEngine`, the on-disk layout and crash contract.
- [`zaino-index-compact-block`](./packages/zaino-index-compact-block/usage.md): height → gRPC-framed compact block.
- [`zaino-index-utxo-set`](./packages/zaino-index-utxo-set/usage.md): unspent outputs' values (compact-block fees).
- [`zaino-index-tree-state`](./packages/zaino-index-tree-state/usage.md): commitment-tree states and subtree roots.
- [`zaino-index-transparent-address`](./packages/zaino-index-transparent-address/usage.md): the t-address RPCs.
- [`zaino-internal-block-hash-to-height`](./packages/zaino-internal-block-hash-to-height/usage.md): the hash → height locator.
- [`zaino-internal-txid-location`](./packages/zaino-internal-txid-location/usage.md): where each mined transaction sits.
- [`zaino-source`](./packages/zaino-source/usage.md): the `ChainDataSource` port, its JSON-RPC and indexer-gRPC clients, `MockChain`.
- [`zaino-traffic`](./packages/zaino-traffic/usage.md): one scheduler for every request to validators and peers.
- [`zaino-peers`](./packages/zaino-peers/usage.md): zebra-network embedded as Zaino's p2p layer.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): domain types and protocol constants.
- [`zaino-proto`](./packages/zaino-proto/README.md): vendored lightwallet and Zebra indexer protocols.

## Security disclosure

Report security issues privately, never in a public issue: see
[CONTRIBUTING.md](./CONTRIBUTING.md#security-issues).

## License

[Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0); see [LICENSE](./LICENSE).
