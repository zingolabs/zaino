# Zaino RPC APIs

`zainod` serves one API: the lightwalletd-compatible `CompactTxStreamer` gRPC
service defined by the
[LightWallet Protocol](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto)
(message types in
[compact_formats.proto](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/compact_formats.proto)).
It listens on `serve.grpc_listen_address` (default `127.0.0.1:8137`) over
plaintext HTTP/2. `zainod` does not serve a JSON-RPC API.

## Served methods

Blocks are served from the finalised store combined with the non-finalised
chain-head window.

- GetLatestBlock (ChainSpec) returns (BlockID)
- GetBlock (BlockID) returns (CompactBlock)
- GetBlockRange (BlockRange) returns (stream CompactBlock)
- GetLightdInfo (Empty) returns (LightdInfo)

## Not yet served

`SendTransaction` is accepted but always answers with a non-zero `error_code`
(transaction relay is not wired). Every other `CompactTxStreamer` method returns
gRPC `UNIMPLEMENTED`:

- GetBlockNullifiers, GetBlockRangeNullifiers
- GetTransaction
- GetTaddressTxids, GetTaddressTransactions, GetTaddressBalance, GetTaddressBalanceStream
- GetMempoolTx, GetMempoolStream
- GetTreeState, GetLatestTreeState, GetSubtreeRoots
- GetAddressUtxos, GetAddressUtxosStream
- Ping
