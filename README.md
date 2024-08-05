# Zingo-Indexer
A rust implemented, nym enhanced, lightwalletd for Zcash.

A note to developers/consumers/contributers: The end goal is not an exact one-to-one port of all existing lwd functionaliy.
We currently plan to hold the Service and Darkside RPC implementations, along with a Nym counterpart to the service RPCs for sending and recieving currency over the Nym Mixnet.

# Security Vulnerability Disclosure
If you believe you have discovered a security issue, please contact us at:

zingodisclosure@proton.me

# Zingo-RPC
Will eventually hold the rust implementations of the LightWallet Service and Darkside RPCs, along with the wallet-side and server-side Nym Service implementations.

# Zingo-IndexerD
Currently a lightweight gRPC server for testing and development. Zingo-IndexerD also has a basic nym server, currently only receives send_transaction and get_lightd_info commands send over the mixnet. 
This should not be used to run mainnet nodes in its current form as it lacks the queueing and error checking logic necessary.

Under the "nym_poc" feature flag Zingo-IndexerD can also act as a Nym powered indexer between zcash wallets and Zingo-IndexerD, capable of sending zcash transactions over the Nym Mixnet. 
Note: The wallet-side nym service RPC implementations are moving to CompactTxStreamerClient for easier consumption by wallets. Functionality under the "nym_poc" feature flag will be removed once a working example has been implemented directly in zingolib.

This is the POC and initial work on enabling zcash infrastructure to use the nym mixnet.

[Nym_POC](./docs/nym_poc.pdf) shows the current state of this work ands our vision for the future. 

Our plan is to first enable wallets to send and recieve transactions via a nym powered indexer between wallets and a lightwalletd/zebrad before looking at the wider zcash ecosystem.


# Dependencies
1) zebrad <https://github.com/ZcashFoundation/zebra.git>
2) lightwalletd <https://github.com/zcash/lightwalletd.git> [require for testing]
3) zingolib <https://github.com/zingolabs/zingolib.git> [if running zingo-cli]
4) zcashd, zcash-cli <https://github.com/zcash/zcash> [required for integration tests until zebrad has working regtest mode]


# Testing
- To run tests:
1) Simlink or copy compiled `zcashd`, `zcash-cli` and `lightwalletd` binaries to `$ zingo-indexer/zingo-testutils/test_binaries/bins/`
3) Run `$ cargo nextest run` or `$ cargo test`

# zingoindexerd
- To run zingo-cli through zingo-indexer, connecting to lightwalletd/zebrad locally:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
3) Run `$ cargo run`

From zingolib:
4) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8080" --data-dir ~/wallets/test_wallet`

# Nym POC
The walletside Nym implementations are moving to ease wallet integration but the POC walletside nym server is still available under the "nym_poc" feature flag.
- To run the POC:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
3) Run `$ cargo run`
4) Copy nym address displayed
5) Run `$ cargo run --features "nym_poc" -- <nym address copied>`

From zingolib: [get_lightd_info and send_transaction commands sent with this build will be sent over the mixnet]
6) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8088" --data-dir ~/wallets/testnet_wallet`

