# Zaino
The Zaino repo consists of several crates that collectively provide an indexing service and APIs for the Zcash blockchain. The crates are modularized to separate concerns, enhance maintainability, and allow for flexible integration.

### Crates
  - `Zainod`
  - `Zaino-Serve`
  - `Zaino-State`
  - `Zaino-Fetch`
  - `Zaino-Proto`
  - `Zaino-Testutils`
  - `Integration-tests`

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


**Zingo-infra-testutils:**
- zingo-infra-testutils
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
- jsonrpc-core 
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
- portpicker
- tempfile


Below is a detailed specification for each crate.

A full specification of the public functionality and RPC services available in Zaino is available in [Cargo Docs](https://zingolabs.github.io/zaino/index.html) and [RPC API Spec](./rpc_api.md).


## ZainoD
`ZainoD` is the main executable that runs the Zaino indexer gRPC service. It serves as the entry point for deploying the Zaino service, handling configuration and initialization of the server components.

### Functionality
- Service Initialization:
  - Parses command-line arguments and configuration files.
  - Initializes the gRPC server and internal caching systems using components from `zaino-serve` and `zaino-state` (backed by `zaino-fetch`).
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

- Error Handling:
  - Maps internal errors to appropriate gRPC status codes.
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

Full documentation for `Zaino-State` can be found [here](https://zingolabs.github.io/zaino/zaino_state/index.html).


## Zaino-Fetch
`Zaino-Fetch` is a library that provides access to the mempool and blockchain data using Zcash's JsonRPC interface. It is primarily used as a backup and for backward compatibility with systems that rely on RPC communication such as `Zcashd`.

### Functionality
- RPC Client Implementation:
  - Implements a `JSON-RPC` client to interact with `Zebra`'s RPC endpoints.
  - Handles serialization and deserialization of RPC calls.

- Data Retrieval and Transaction Submission:
  - Fetches blocks, transactions, and mempool data via RPC.
  - Sends transactions to the network using the `sendrawtransaction` RPC method.

- Block and Transaction Deserialisation logic:
  - Provides Block and transaction deserialisation implementaions.

- Mempool and CompactFormat access:
  - Provides a simple mempool implementation for use in gRPC service implementations. (This is due to be refactored and possibly moved with the development of `Zaino-State`.)
  - Provides parse implementations for converting "full" blocks and transactions to "compact" blocks and transactions.

- Fallback Mechanism:
  - Acts as a backup when direct access via `zaino-state` is unavailable.

Full documentation for `Zaino-Fetch` can be found [here](https://zingolabs.github.io/zaino/zaino_fetch/index.html).


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
