# Zaino
Zaino is an indexer for the Zcash blockchain implemented in Rust.

Zaino provides all necessary functionality for "light" clients (wallets and other applications that don't rely on the complete history of blockchain) and "full" clients / wallets and block explorers providing access to both the finalized chain and the non-finalized best chain and mempool held by either a Zebra or Zcashd full validator.


### Motivations
With the ongoing Zcashd deprecation project, there is a push to transition to a modern, Rust-based software stack for the Zcash ecosystem. By implementing Zaino in Rust, we aim to modernize the codebase, enhance performance and improve overall security. This work will build on the foundations laid down by [Librustzcash](https://github.com/zcash/librustzcash) and [Zebra](https://github.com/ZcashFoundation/zebra), helping to ensure that the Zcash infrastructure remains robust and maintainable for the future.

Due to current potential data leaks / security weaknesses highlighted in [revised-nym-for-zcash-network-level-privacy](https://forum.zcashcommunity.com/t/revised-nym-for-zcash-network-level-privacy/46688) and [wallet-threat-model](https://zcash.readthedocs.io/en/master/rtd_pages/wallet_threat_model.html), there is a need to use anonymous transport protocols (such as Nym or Tor) to obfuscate clients' identities from Zcash's indexing servers ([Lightwalletd](https://github.com/zcash/lightwalletd), [Zcashd](https://github.com/zcash/zcash), Zaino). As Nym has chosen Rust as their primary SDK ([Nym-SDK](https://github.com/nymtech/nym)), and Tor is currently implementing Rust support ([Arti](https://gitlab.torproject.org/tpo/core/arti)), Rust is a straightforward and well-suited choice for this software.

Zebra has been designed to allow direct read access to the finalized state and RPC access to the non-finalized state through its ReadStateService. Integrating directly with this service enables efficient access to chain data and allows new indices to be offered with minimal development.

Separation of validation and indexing functionality serves several purposes. First, by removing indexing functionality from the Validator (Zebra) will lead to a smaller and more maintainable codebase. Second, by moving all indexing functionality away from Zebra into Zaino will unify this paradigm and simplify Zcash's security model. Separating these concerns (consensus node and blockchain indexing) serves to create a clear trust boundary between the Indexer and Validator allowing the Indexer to take on this responsibility. Historically, this had been the case for "light" clients/wallets using [Lightwalletd](https://github.com/zcash/lightwalletd) as opposed to "full-node" client/wallets and block explorers that were directly served by the [Zcashd full node](https://github.com/zcash/zcash).


### Goals
Our primary goal with Zaino is to serve all non-miner clients -such as wallets and block explorers- in a manner that prioritizes security and privacy while also ensuring the time efficiency critical to a stable currency. We are committed to ensuring that these clients can access all necessary blockchain data and services without exposing sensitive information or being vulnerable to attacks. By implementing robust security measures and privacy protections, Zaino will enable users to interact with the Zcash network confidently and securely.

To facilitate a smooth transition for existing users and developers, Zaino is designed (where possible) to maintain backward compatibility with Lightwalletd and Zcashd. This means that applications and services currently relying on these platforms can switch to Zaino with minimal adjustments. By providing compatible APIs and interfaces, we aim to reduce friction in adoption and ensure that the broader Zcash ecosystem can benefit from Zaino's enhancements without significant rewrites or learning curves.

### Scope
Zaino will implement a comprehensive RPC API to serve all non-miner client requests effectively. This API will encompass all functionality currently in the LightWallet gRPC service ([CompactTxStreamer](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto)), currently served by Lightwalletd, and a subset of the [Zcash RPCs](https://zcash.github.io/rpc/) required by wallets and block explorers, currently served by Zcashd. Zaino will unify these two RPC services and provide a single, straightforward interface for Zcash clients and service providers to access the data and services they require.

In addition to the RPC API, Zaino will offer a client library allowing developers to integrate Zaino's functionality directly into their Rust applications. Along with the RemoteReadStateService mentioned below, this will allow both local and remote access to the data and services provided by Zaino without the overhead of using an RPC protocol, and also allows Zebra to stay insulated from directly interfacing with client software.

Currently Zebra's `ReadStateService` only enables direct access to chain data (both Zebra and any process interfacing with the `ReadStateService` must be running on the same hardware). Zaino will extend this functionality, using a Hyper wrapper, to allow Zebra and Zaino (or software built using Zaino's `IndexerStateService` as its backend) to run on different hardware and should enable a much greater range of deployment strategies (eg. running validator, indexer or wallet processes on separate hardware). It should be noted that this will primarily be designed as a remote link between Zebra and Zaino and it is not intended for developers to directly interface with this service, but instead to use functionality exposed by the client library in Zaino (`IndexerStateService`).


## Documentation
- [Use Cases](./docs/use_cases.md): Holds instructions and example use cases.
- [Testing](./docs/testing.md): Holds instructions for running tests.
- [Live Service System Architecture](./docs/zaino_live_system_architecture.pdf): Holds the Zcash system architecture diagram for the Zaino live service.
- [Library System Architecture](./docs/zaino_lib_system_architecture.pdf): Holds the Zcash system architecture diagram for the Zaino client library.
- [ZainoD (Live Service) Internal Architecture](./docs/zaino_serve_architecture_v020.pdf): Holds an internal Zaino system architecture diagram.
- [Zaino-State (Library) Internal Architecture](./docs/zaino_state_architecture_v020.pdf): Holds an internal Zaino system architecture diagram.
- [Internal Specification](./docs/internal_spec.md): Holds a specification for Zaino and its crates, detailing their functionality, interfaces and dependencies.
- [RPC API Spec](./docs/rpc_api.md): Holds a full specification of all of the RPC services served by Zaino.
- [Cargo Docs](https://zingolabs.github.io/zaino/): Holds a full code specification for Zaino.


## Security Vulnerability Disclosure
If you believe you have discovered a security issue, please contact us at:

zingodisclosure@proton.me


## License
This project is licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0). See the [LICENSE](./LICENSE) file for details.
