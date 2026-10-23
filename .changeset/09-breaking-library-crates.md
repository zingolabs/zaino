---
default: major
---

# Library crates: new workspace layout

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
