# Zaino
The Zaino repo consists of several crates that collectively provide an indexing service and APIs for the Zcash blockchain. The crates are modularized to separate concerns, enhance maintainability, and allow for flexible integration.

### Crates
In dependency order. The source stack (`zaino-primitives` through
`zaino-source-zebra`) is described by [ADR-0008](./adr/0008-source-ports-and-domain-primitives.md);
each of its crates carries a `usage.md` beside its `Cargo.toml`.
  - `Zaino-Primitives` — domain vocabulary
  - `Zaino-Address` — address classification
  - `Zaino-Source` — driven ports
  - `Zaino-Rpc` — JSON-RPC transport
  - `Zaino-Convert-Zebra` — zebra-chain → domain conversions
  - `Zaino-Source-Zebra-Rpc` — JSON-RPC adapter
  - `Zaino-Source-Zebra-Readstate` — ReadStateService adapter
  - `Zaino-Source-Zebra` — the validator composite
  - `Zaino-Common`
  - `Zaino-Proto`
  - `Zaino-State`
  - `Zaino-Serve`
  - `Zainod`
  - `Zaino-Testutils`, `clientless`, `e2e` — the live-test suite

### Workspace Dependencies
**Zingo Labs:**
- zingolib
- testvectors

**Librustzcash:**
- zcash_client_backend
- zcash_protocol


**Zebra:**
- zebra-chain
- zebra-state
- zebra-rpc


**Zingo-infra-services:**
- zingo-infra-services

**Runtime:**
- tokio
- tokio-stream

**CLI:**
- clap

**Tracing:**
- tracing
- tracing-subscriber
- tracing-futures

**Network / RPC:**
- http
- url
- reqwest
- tower
- tonic
- tonic-build
- prost
- serde
- serde_json
- jsonrpsee-core
- jsonrpsee-types

**Hashmaps, channels, DBs:**
- indexmap
- crossbeam-channel
- dashmap
- lmdb

**Async:**
- async-stream
- async-trait
- futures

**Utility:**
- thiserror
- lazy-regex
- once_cell
- ctrlc
- chrono
- which
- whoami

**Formats:**
- base64
- byteorder
- sha2
- hex
- toml

**Test:**
- tempfile


Below is a detailed specification for each crate.

A full specification of the public functionality and RPC services available in Zaino is available in [Cargo Docs](https://zingolabs.github.io/zaino/index.html) and [RPC API Spec](./rpc_api.md).


## ZainoD
`ZainoD` is the main executable that runs the Zaino indexer gRPC service. It serves as the entry point for deploying the Zaino service, handling configuration and initialization of the server components.

### Functionality
- Service Initialization:
  - Parses command-line arguments and configuration files.
  - Initializes the gRPC and JSON-RPC servers and internal caching systems using components from `zaino-serve` and `zaino-state` (backed by the `zaino-source-zebra` composite).
  - Sets up logging and monitoring systems.

- Runtime Management:
  - Manages the asynchronous runtime using `Tokio`.
  - Handles graceful shutdowns and restarts.

Full documentation for `ZainoD` can be found [here](https://zingolabs.github.io/zaino/zainod/index.html) and [here](https://zingolabs.github.io/zaino/zainodlib/index.html).


## Zaino-Serve
`Zaino-Serve` contains the gRPC server and the Rust implementations of the LightWallet gRPC service (`CompactTxStreamerServer`). It handles incoming client requests and interacts with backend services to fulfill them.

### Functionality
- gRPC Server Implementation:
  - Utilizes `Tonic` to implement the gRPC server.
  - Hosts the `CompactTxStreamerServer` service for client interactions.

- `CompactTxStreamerServer` Method Implementations:
  - Implements the full set of methods as defined in the [LightWallet Protocol](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto).

- Request Handling:
  - Validates and parses client requests.
  - Communicates with `zaino-state` to retrieve data.

- The served JSON-RPC schema (`rpc/jsonrpc/wire/`):
  - Owns the JSON shape Zaino emits for its zcashd-compatible RPC surface —
    serde structs with zcashd's exact field names, one `from_domain` conversion
    per type, and golden serialization tests beside each.
  - This is deliberately *not* shared with the shape Zaino accepts from a
    validator, which is `zaino-source-zebra-rpc`'s. The two interfaces
    genuinely differ, and one type serving both directions cannot express that.
    See [ADR-0009](./adr/0009-served-json-schema-lives-in-zaino-serve.md).

- Error Handling:
  - Maps internal errors to appropriate gRPC status codes.
  - Recovers zcashd-compatible legacy error codes by walking the error chain for
    `zaino_source::FetchError` (a code the *validator* returned) or
    `zaino_state::LegacyRpcError` (Zaino's own rejection).
  - Provides meaningful error messages to clients.

Full documentation for `Zaino-Serve` can be found [here](https://zingolabs.github.io/zaino/zaino_serve/index.html).


## Zaino-State
`Zaino-State` is Zaino's chain fetch and transaction submission library, interfacing with zcash validators throught a configurable backend. It is designed for direct consumption by full node wallets and internal services, enabling a simlified interface for Zcash clients.

### Functionality
- Blockchain Data Access:
  - Fetches finalized and non-finalized state data.
  - Retrieves transaction data and block headers.
  - Accesses chain metadata like network height and difficulty.

- Mempool Management:
  - Interfaces with the mempool to fetch pending transactions.
  - Provides efficient methods to monitor mempool changes.

- Chain Synchronization:
  - Keeps track of the chain state in sync with Zebra.
  - Handles reorgs and updates to the best chain.

- Caching Mechanisms:
  - Implements caching for frequently accessed data to improve performance.

- Configurable Backend:
  - Implementes a configurable backend service enabling clients to use a single interface for any validator set-up.

- Finalised State (persistent or ephemeral):
  - The finalised portion of the chain index (all but the top 100 blocks) is
    served by a `FinalisedState` facade over a `FinalisedSource` backing. The
    backing is either a versioned, LMDB-backed persistent database or, when
    `ephemeral_finalised_state` is set, an ephemeral passthrough that serves
    finalised reads directly from the backing validator and persists nothing.
  - Sync and version migration are **background, non-blocking** operations. Large
    syncs and migrations run in the background while an ephemeral passthrough
    continues serving finalised reads from the source; small syncs run inline.
    Background failures retry and escalate to a critical status.
  - During a large background sync/migration, passthrough-served blocks carry a
    chainwork of `0`. This is consistent for the non-finalised state's relative
    fork-choice but leaves absolute chainwork offset-low until the persistent
    database catches up. The non-finalised cache caps its in-memory retention at
    a fixed depth below the tip so it cannot grow unbounded while `db_height`
    lags or is pinned at `0`.

Full documentation for `Zaino-State` can be found [here](https://zingolabs.github.io/zaino/zaino_state/index.html).


## The source stack (`zaino-primitives` … `zaino-source-zebra`)
Validator access is a hexagonal port/adapter stack rather than a single client
library. It replaces `Zaino-Fetch`, which was deleted in this cycle. See
[ADR-0008](./adr/0008-source-ports-and-domain-primitives.md) for the reasoning
and each crate's `usage.md` for practical guidance.

### Functionality
- Domain vocabulary (`zaino-primitives`):
  - The chain in Zaino's own terms: blocks, transactions, hashes, heights,
    treestates, amounts, and the passthrough RPC response shapes.
  - Depends on `thiserror` and nothing else. No serde: serialization belongs to
    whichever boundary owns the format.

- Ports (`zaino-source`):
  - One trait per question a consumer can ask, each with its own error type.
  - `QueryError` separates a *domain answer* (the validator replied; not
    retried) from a *transport failure* (retried by `Resilient`, according to a
    machine-readable `FailureMode`). This is what lets a zcashd legacy error
    code survive from the validator to the served response.
  - Capability is structural: an adapter that cannot answer a question does not
    implement its port, so mis-routing is a compile error.

- Transport (`zaino-rpc`):
  - HTTP, the JSON-RPC envelope, authentication, and retry-on-`-1`.
  - `call()` returns a raw `serde_json::Value`; parsing is the adapter's job.
    This is what lets the same client serve both the production adapter and the
    live tests' independent oracle.

- Adapters (`zaino-source-zebra-rpc`, `zaino-source-zebra-readstate`):
  - The JSON-RPC adapter implements every port JSON-RPC can answer, and owns
    response parsing (Zaino's external-input validation) and error
    classification.
  - The read-state adapter reads Zebra's state database directly where Zaino
    and Zebra share a host. It is an accelerator, not an alternative, and
    deliberately does not implement the mempool or passthrough ports.

- Composite (`zaino-source-zebra`):
  - `ZebraValidator` holds an RPC adapter and an optional read-state adapter,
    and routes each question to whichever can answer it. RPC-only and
    RPC+read-state are configurations of one type rather than variants of an
    enum.


## Zaino-Proto
`Zaino-Proto` contains the `Tonic`-generated code for the LightWallet service RPCs and compact formats. It holds the protocol buffer definitions and the generated Rust code necessary for gRPC communication.

### Functionality
- Protocol Definitions:
  - `.proto` files defining the services and messages for LightWalletd APIs.
  - Includes definitions for compact blocks, transactions, and other data structures.

- Code Generation:
  - Uses `prost` to generate Rust types from `.proto` files.
  - Generates client and server stubs for gRPC services.

* We plan to eventually rely on `LibRustZcash`'s versions but hold our own here for development purposes.


## Zaino-Testutils and Integration-Tests
The `Zaino-Testutils` and `Integration-Tests` crates are dedicated to testing the Zaino project. They provide utilities and comprehensive tests to ensure the correctness, performance, and reliability of Zaino's components.
- `Zaino-Testutils`: This crate contains common testing utilities and helper functions used across multiple test suites within the Zaino project.
- `Integration-Tests`: This crate houses integration tests that validate the interaction between different Zaino components and external services like `Zebra` and `Zingolib`.

### Test Modules
- `wallet_to_validator`: Holds Wallet-to-Validator tests that test Zaino's functionality within the compete software stack.
- `client_rpcs`: Holds RPC tests that test the functionality of the LightWallet gRPC services in Zaino and compares the outputs with the corresponding services in `Lightwalletd` to ensure compatibility.

Full documentation for `Zaino-Testutils` can be found [here](https://zingolabs.github.io/zaino/zaino_testutils/index.html).
