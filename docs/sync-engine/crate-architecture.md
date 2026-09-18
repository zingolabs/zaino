## Crate Architecture

The sync engine follows a hexagonal (ports-and-adapters) architecture.
Domain core crates define business logic with no external dependencies.
Port crates declare trait interfaces. Adapter crates implement those
traits against concrete infrastructure. Orchestration wires indexes
together. Shared infrastructure provides cross-cutting utilities.

```mermaid
graph TB
    subgraph domain_core ["Domain Core"]
        primitives["zaino-primitives<br/><i>Domain types</i>"]
        sync["zaino-sync<br/><i>DAG sync engine</i>"]
    end

    subgraph ports ["Ports (Trait Crates)"]
        source["zaino-source<br/><i>Driven port: validator queries</i>"]
        persistence["zaino-persistence<br/><i>Driven port: storage</i>"]
    end

    subgraph adapters ["Adapters"]
        zebra_rpc["zaino-source-zebra-rpc<br/><i>Zebra JSON-RPC adapter</i>"]
        zebra_rs["zaino-source-zebra-readstate<br/><i>Zebra ReadState adapter</i>"]
        lmdb["zaino-backend-lmdb<br/><i>LMDB storage adapter</i>"]
    end

    subgraph orchestration ["Orchestration"]
        indexes["zaino-indexes<br/><i>Zcash index definitions + sets</i>"]
    end

    subgraph infra ["Shared Infrastructure"]
        rpc["zaino-rpc<br/><i>JSON-RPC 2.0 client</i>"]
        convert["zaino-convert-zebra<br/><i>zebra-chain to domain conversion</i>"]
    end

    %% Port dependencies on domain core
    source -->|"depends on"| primitives
    persistence -->|"depends on"| primitives

    %% Engine depends on ports + domain
    sync -->|"depends on"| persistence
    sync -->|"depends on"| primitives

    %% Orchestration depends on engine + domain
    indexes -->|"depends on"| sync
    indexes -->|"depends on"| primitives

    %% Adapter: zaino-source-zebra-rpc
    zebra_rpc -.->|"implements<br/>GetBlock, GetChainTip,<br/>GetTreestate"| source
    zebra_rpc -->|"depends on"| primitives
    zebra_rpc -->|"depends on"| rpc
    zebra_rpc -->|"depends on"| convert

    %% Adapter: zaino-source-zebra-readstate
    zebra_rs -.->|"implements<br/>GetBlock, GetChainTip"| source
    zebra_rs -->|"depends on"| primitives
    zebra_rs -->|"depends on"| convert

    %% Adapter: zaino-backend-lmdb
    lmdb -.->|"implements<br/>Backend, BackendReader,<br/>BackendWriter"| persistence

    %% Infra dependencies
    rpc -->|"depends on"| source
    convert -->|"depends on"| primitives

    %% External dependencies (outside the workspace)
    zebra_rpc -.->|"uses"| zebra_chain["zebra-chain"]
    zebra_rs -.->|"uses"| zebra_chain
    zebra_rs -.->|"uses"| zebra_state["zebra-state"]
    convert -.->|"uses"| zebra_chain
    lmdb -.->|"uses"| lmdb_ext["lmdb (C lib)"]

    %% Styling
    classDef port fill:#e8f4f8,stroke:#2196F3,stroke-width:2px
    classDef adapter fill:#fff3e0,stroke:#FF9800,stroke-width:2px
    classDef core fill:#e8f5e9,stroke:#4CAF50,stroke-width:2px
    classDef orch fill:#f3e5f5,stroke:#9C27B0,stroke-width:2px
    classDef shared fill:#fce4ec,stroke:#E91E63,stroke-width:1px
    classDef external fill:#f5f5f5,stroke:#9E9E9E,stroke-width:1px,stroke-dasharray: 5 5

    class source,persistence port
    class zebra_rpc,zebra_rs,lmdb adapter
    class primitives,sync core
    class indexes orch
    class rpc,convert shared
    class zebra_chain,zebra_state,lmdb_ext external
```

### Crate Summary

| Crate | Layer | Role | Implements |
|-------|-------|------|------------|
| `zaino-primitives` | Domain Core | Zcash domain types: `Block`, `BlockHeader`, `Transaction`, `Height`, `BlockHash`, `TransactionHash`, `Zatoshis`, etc. Zero external dependencies beyond `thiserror`. | -- |
| `zaino-sync` | Domain Core | DAG-driven parallel sync engine. Defines the `IndexDef` trait hierarchy (scope axis: `BlockLocal` / `SelfCumulative` / `CrossIndex`; composition axis: `Append` / `Monoidal` / `Fold`), the `Schema` trait for persistence encoding, scheduler, block buffer, and pipeline. Generic -- contains no blockchain knowledge. | -- |
| `zaino-source` | Port (Driven) | One trait per validator query: `GetBlock`, `GetChainTip`, `GetBestBlockHeight`, `GetTransaction`, `GetBlockByHash`, `GetBlockVerbose`, `GetTreestate`, `GetAddressBalance`, `GetAddressDeltas`, `GetAddressTxids`, `GetAddressUtxos`, `GetMempoolTxids`, `GetSubtreeRoots`, `GetCommitmentTreeRoots`. Also provides `Resilient` wrapper and `RetryPolicy`. | -- |
| `zaino-persistence` | Port (Driven) | Storage abstraction: `Backend` (open reader/writer/flush), `BackendReader` (get/scan by namespace), `BackendWriter` (atomic commit of `WriteOp` batches). Namespace-based keyspace organization. | -- |
| `zaino-source-zebra-rpc` | Adapter (Source) | Bridges `zaino-source` traits to Zebra's JSON-RPC endpoint. Uses `zaino-rpc` for transport and `zaino-convert-zebra` for type mapping. | `GetBlock`, `GetChainTip`, `GetTreestate` |
| `zaino-source-zebra-readstate` | Adapter (Source) | Bridges `zaino-source` traits to Zebra's finalized state DB via `zebra-state` `ReadStateService` (Tower). Direct DB reads -- no HTTP, no JSON. Orders of magnitude faster for bulk sync. | `GetBlock`, `GetChainTip` |
| `zaino-backend-lmdb` | Adapter (Persistence) | LMDB-backed storage. One named database per `Namespace`. Zero-copy reads via memory-mapped files. Atomic batch commits. `NO_SYNC` mode with explicit flush at batch boundaries. | `Backend`, `BackendReader`, `BackendWriter` |
| `zaino-indexes` | Orchestration | Zcash-specific index definitions (headers, txids, txid-location, hash-to-height, transparent-spends, transparent-data, sapling, orchard) and pre-composed index sets (headers-only, headers-and-spends, current-zaino). Each index implements `IndexDef` + extract + merge + `Schema`. Sets define `ProvideContext` projections and builder functions. | `IndexDef`, `Extract*`, `Merge*`, `Schema` |
| `zaino-rpc` | Shared Infra | JSON-RPC 2.0 client: HTTP transport, request/response envelope, retry on work-queue exhaustion, authentication. Returns raw `serde_json::Value` -- response parsing is the adapter's responsibility. | -- |
| `zaino-convert-zebra` | Shared Infra | Pure conversion functions: `zebra-chain` types to `zaino-primitives` domain types. `block_from_zebra`, `header_from_zebra`, `header_from_parts` entry points composing per-type converters. | -- |

### Dependency Flow

The dependency graph enforces the hexagonal architecture invariant:

- **Inward only**: adapters depend on ports, ports depend on domain core. Never the reverse.
- **No adapter-to-adapter**: source adapters and the persistence adapter are fully independent.
- **Engine is generic**: `zaino-sync` depends on `zaino-persistence` (the port), never on `zaino-backend-lmdb` (the adapter). It depends on `zaino-primitives` for `IndexId` only.
- **Orchestration is the composition root**: `zaino-indexes` depends on `zaino-sync` and `zaino-primitives` to define concrete Zcash indexes. The application entry point selects adapters and wires them together.
- **Conversion is infra, not domain**: `zaino-convert-zebra` depends on `zaino-primitives` (domain) and `zebra-chain` (external). It is used by source adapters, not by the engine.

---

- [Formal Model](formal-model.md)
- [Design Spec](design.md)
- [Implementation Spec](implementation.md)
- [User Stories](user-stories.md)
