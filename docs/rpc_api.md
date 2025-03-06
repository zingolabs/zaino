# Zaino RPC APIs
## Lightwallet gRPC Services
Zaino Currently Serves the following gRPC services as defined in the [LightWallet Protocol](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto):
  - GetLatestBlock (ChainSpec) returns (BlockID)
  - GetBlock (BlockID) returns (CompactBlock)
  - GetBlockNullifiers (BlockID) returns (CompactBlock)
  - GetBlockRange (BlockRange) returns (stream CompactBlock)
  - GetBlockRangeNullifiers (BlockRange) returns (stream CompactBlock)
  - GetTransaction (TxFilter) returns (RawTransaction)
  - SendTransaction (RawTransaction) returns (SendResponse)
  - GetTaddressTxids (TransparentAddressBlockFilter) returns (stream RawTransaction)
  - GetTaddressBalance (AddressList) returns (Balance)
  - GetTaddressBalanceStream (stream Address) returns (Balance) (**MARKED FOR DEPRECATION**)
  - GetMempoolTx (Exclude) returns (stream CompactTx)
  - GetMempoolStream (Empty) returns (stream RawTransaction)
  - GetTreeState (BlockID) returns (TreeState)
  - GetLatestTreeState (Empty) returns (TreeState)
  - GetSubtreeRoots (GetSubtreeRootsArg) returns (stream SubtreeRoot)
  - GetAddressUtxos (GetAddressUtxosArg) returns (GetAddressUtxosReplyList)
  - GetAddressUtxosStream (GetAddressUtxosArg) returns (stream GetAddressUtxosReply)
  - GetLightdInfo (Empty) returns (LightdInfo)
  - Ping (Duration) returns (PingResponse) (**CURRENTLY UNIMPLEMENTED**)


## Zcash RPC Services
Zaino has also committed to taking over responsibility for serving all [Zcash RPC Services](https://zcash.github.io/rpc/) required by non-validator (miner) clients from Zcashd.
A full specification of the Zcash RPC services served by Zaino, and their current state of development, can be seen [here](./Zaino-zcash-rpcs.pdf).

  
