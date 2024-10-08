# Zaino
A(n eventual) rust implemented, nym enhanced, indexer and lightwallet service for Zcash.

Zaino is intended to provide all necessary funtionality for clients, including "standalone" (formerly "light") clients/wallets and integrated (formerly "full") client/wallets to access both the finalized chain and non-finalized best chain and the mempool, held by a Zebrad full validator.

*A note to developers/consumers/contributers: The end goal is not an exact one-to-one port of all existing lwd functionality. We currently plan to hold the Service and Darkside RPC implementations, along with a Nym counterpart to the service RPCs for sending and recieving currency over the Nym Mixnet.

# Security Vulnerability Disclosure
If you believe you have discovered a security issue, please contact us at:

zingodisclosure@proton.me

# ZainoD
The Zaino Indexer service.

Under the "nym_poc" feature flag ZainoD can also act as a Nym powered proxy, running between zcash wallets and Zingo-IndexerD, capable of sending zcash transactions over the Nym Mixnet. 
Note: The wallet-side nym service RPC implementations are moving to CompactTxStreamerClient for easier consumption by wallets. Functionality under the "nym_poc" feature flag will be removed once a working example has been implemented directly in zingolib.

This is the POC and initial work on enabling zcash infrastructure to use the nym mixnet.

# Zaino-Serve
Holds a gRPC server capable of servicing clients over both https and the nym mixnet.

Also holds the rust implementations of the LightWallet Service (CompactTxStreamerServer) and (eventually) Darkside RPCs (DarksideTxStremerServer).

* Currently only send_transaction and get_lightd_info are implemented over nym.

# Zaino-Wallet [*Temporarily Removed due to Nym Dependency Conflict]
Holds the nym-enhanced, wallet-side rust implementations of the LightWallet Service RPCs (NymTxStreamerClient).

* Currently only send_transaction and get_lightd_info are implemented over nym.

# Zaino-State
A mempool and chain-fetching service built on top of zebra's ReadStateService and TrustedChainSync, exosed as a library for direct consumption by full node wallets.

# Zaino-Fetch
A mempool-fetching, chain-fetching and transaction submission service that uses zebra's RPC interface. Used primarily as a backup and legacy option for backwards compatibility.

# Zaino-Nym [*Temporarily Removed due to Nym Dependency Conflict]
Holds backend nym functionality used by Zaino.

# Zaino-Proto
Holds tonic generated code for the lightwallet service RPCs and compact formats.

* We plan to eventually rely on LibRustZcash's versions but hold our our here for development purposes.


# Dependencies
1) zebrad <https://github.com/ZcashFoundation/zebra.git>
2) lightwalletd <https://github.com/zcash/lightwalletd.git> [require for testing]
3) zingolib <https://github.com/zingolabs/zingolib.git> [if running zingo-cli]
4) zcashd, zcash-cli <https://github.com/zcash/zcash> [required until switch to zebras regtest mode]


# Testing
- To run tests:
1) Simlink or copy compiled `zcashd`, `zcash-cli` and `lightwalletd` binaries to `$ zingo-indexer/zingo-testutils/test_binaries/bins/*`
3) Run `$ cargo nextest run` or `$ cargo test`

# Running ZainoD
- To run zingo-cli through Zaino, connecting to zebrad locally: [in seperate terminals]
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
3) Run `$ cargo run`

From #PATH_TO/zingolib:
4) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8080" --data-dir ~/wallets/test_wallet`

# Nym POC [*Temporarily Removed due to Nym Dependency Conflict]
The walletside Nym implementations are moving to ease wallet integration but the POC walletside nym server is still available under the "nym_poc" feature flag.
- To run the POC [in seperate terminals]:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
3) Run `$ cargo run`
4) Copy nym address displayed
5) Run `$ cargo run --features "nym_poc" -- <nym address copied>`

From #PATH_TO/zingolib: [send_transaction commands sent with this build will be sent over the mixnet]
6) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8088" --data-dir ~/wallets/testnet_wallet`

Note:
Configuration data can be set using a .toml file (an example zindexer.toml is given in zingo-indexer/zindexer.toml) and can be set at runtime using the --config arg:
- Run `$ cargo run --config zingo-indexerd/zindexer.toml`

