# Zingo-Proxy
A(n eventual) replacement for lightwalletd, written in Rust.

Currently connects to a lightwalletd, and acts as a man-in-the-middle proxy that does nothing. 
Each RPC we wish to support will be added individually, by connecting to zebrad and doing any nessisary processing.
Eventually, we'll no longer have any calls that need to use the lightwalletd, and it can be removed from the network stack entirely.

A note to developers/consumers/contributers: The end goal is not an exact one-to-one port of all existing lwd functionaliy.
We currently plan to hold the Service and Darkside RPC implementations, along with a Nym counterpart to the service RPCs for sending and recieving currency over the Nym Mixnet. And a Lightweight gRPC server for testing and development (this may be fleshed out to be a mainnet LightWalletD alternative in the future but is currently not a priority and will depend on zebrad).


# Zingo-RPC
will eventually hold the rust implementations of the LightWallet Service and Darkside RPCs, along with the wallet-side and server-side Nym Service implementations.

# Zingo-ProxyD
A lightweight gRPC server for testing and development. This should not be used to run mainnet nodes in its current form as it lacks the queueing and error checking logic necessary.
Zingo-ProxyD also has a basic nym server, gated behind the "nym" feature. Enabling this feature will run both the gRPC server and Nym server.

Under the "nym_poc" feature flag Zingo-ProxyD can also act as a Nym powered proxy between zcash wallets and Zingo-ProxyD, capable of sending zcash transactions over the Nym Mixnet. 
Note: The wallet-side nym service RPC implementations are moving to CompactTxStreamerClient for easier consumption by wallets. Functionality under the "nym_poc" feature flag will be removed once a working example has been implemented directly in zingolib.

This is the POC and initial work on enabling zcash infrastructure to use the nym mixnet.

[Nym_POC](./docs/nym_poc.pdf) shows the current state of this work ands our vision for the future. 

Our plan is to first enable wallets to send and recieve transactions via a nym powered proxy between wallets and a lightwalletd/zebrad before looking at the wider zcash ecosystem.


# Dependencies
1) zebrad <https://github.com/ZcashFoundation/zebra.git>
2) lightwalletd <https://github.com/zcash/lightwalletd.git>
3) zingolib <https://github.com/zingolabs/zingolib.git> [if running zingo-cli]

# zingoproxyd
- To run tests:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
2) Run `$ ./lightwalletd --no-tls-very-insecure --zcash-conf-path $PATH_TO_ZINGO_PROXY/zcash.conf --data-dir . --log-file /dev/stdout`
3) Run `$ cargo nextest run`

- To run zingo-cli through zingo-proxy, connecting to lightwalletd/zebrad locally:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
2) Run `$ ./lightwalletd --no-tls-very-insecure --zcash-conf-path $PATH_TO_ZINGO_PROXY/zcash.conf --data-dir . --log-file /dev/stdout`
3) Run `$ cargo run`, or to activate nym server run `$ cargo run --features "nym"`
From zingolib:
4) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8080" --data-dir ~/wallets/test_wallet`


The walletside Nym implementations are moving to ease wallet integration but the POC walletside nym server is still available under the "nym_poc" feature flag.
- To run the POC:
1) Run `$ zebrad --config #PATH_TO_ZINGO_PROXY/zebrad.toml start`
2) Run `$ ./lightwalletd --no-tls-very-insecure --zcash-conf-path $PATH_TO_ZINGO_PROXY/zcash.conf --data-dir . --log-file /dev/stdout`
3) Run `$ cargo run --features "nym"`
4) Copy nym address displayed
5) Run `$ cargo run --features "nym_poc" -- <nym address copied>`
From zingolib:
6) Run `$ cargo run --release --package zingo-cli -- --chain "testnet" --server "127.0.0.1:8088" --data-dir ~/wallets/testnet_wallet`

