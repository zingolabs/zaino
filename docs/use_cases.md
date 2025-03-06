# Indexer Live Service
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git) or [Zcashd, Zcash-Cli](https://github.com/zcash/zcash.git)
2) [Zingolib](https://github.com/zingolabs/zingolib.git) [if running Zingo-Cli]

### Running ZainoD
- To run a Zaino server, backed locally by Zebrad first build Zaino:
1) Run `$ cargo build --release`
2) Add compiled binary held at `#PATH_TO/zaino/target/release/zainod` to PATH.

- Then to launch Zaino: [in separate terminals]:
3) Run `$ zebrad --config #PATH_TO_CONF/zebrad.toml start`
4) Run `$ zainod --config #PATH_TO_CONF/zindexer.toml`

NOTE: Unless the `no_db` option is set to true in the config file zaino will sync its internal `CompactBlock` cache with the validator it is connected to on launch. This can be a very slow process the first time Zaino's DB is synced with a new chain and zaino will not be operable until the database is fully synced. If Zaino exits during this process the database is saved in its current state, enabling the chain to be synced in several stages.

- To launch Zingo-Cli running through Zaino [from #PATH_TO/zingolib]:
5) Run `$ cargo run --release --package zingo-cli -- --chain "CHAIN_TYPE" --server "ZAINO_LISTEN_ADDR" --data-dir #PATH_TO_WALLET_DATA_DIR`

- Example Config files for running Zebra and Zaino on testnet are given in `zaino/zainod/*`

A system architecture diagram for this service can be seen at [Live Service System Architecture](./zaino_live_system_architecture.pdf).


# Local Library
Zaino-State serves as Zaino's chain fetch and transaction submission library. The intended endpoint for this lib is the `IndexerService<Service>` (and `IndexerServiceSubscriber<ServiceSubscriber>`) held in `Zaino_state::indexer`. This generic endpoint enables zaino to add new backend options (Tonic, Darkside, Nym) to the IndexerService without changing the interface clients will see.

The use of a `Service` and `ServiceSubscriber` separates the core chainstate maintainer processes from fetch fuctionality, enabling zaino to serve a large number of concurrent clients efficiently. In the future we will also be adding a lightweight tonic backend option for clients that do not want to run any chainstate processes locally.

Currently 2 `Service's` are being implemented, with plans for several more:
- FetchService: Zcash JsonRPC powered backend service enabling compatibility with a large number of validator options (zcashd, zebrad).
- StateService: Highly efficient chain fetch service tailored to run with ZebraD.

Future Planned backend Services:
- TonicService: gRPC powered backend enabling lightclients and lightwieght users to use Zaino's unified chain fetch and transaction submission services.
- DarksideService: Local test backend replacing functionality in lightwalletd.
- NymService: Nym powered backend enabling clients to obfuscate their identities from zcash servers.

An example of how to spawn an `IndexerService<FetchService>` and create a `Subscriber` can be seen in `zainod::indexer::Indexer::spawn()`.

A system architecture diagram for this service can be seen at [Library System Architecture](./zaino_lib_system_architecture.pdf).

NOTE: Currently for the mempool to function the `IndexerService` can not be dropped. An option to only keep the `Subscriber` in scope will be added with the addition of the gRPC backend (`TonicService`).

# Remote Library
**Currently Unimplemented, documentation will be added here as this functionality becomes available.**

A system architecture diagram for this service can be seen at [Library System Architecture](./zaino_lib_system_architecture.pdf).
