---
default: major
---

# `zainod`: Zaino serves no JSON-RPC

The JSON-RPC server, its cookie authentication and port 8237 are gone, with every method it
served (`getblock`, `getrawtransaction`, `z_gettreestate`, `getaddressdeltas`, `getblockdeltas`,
…).

**Migration:** send node RPC to the validator's own JSON-RPC. Wallets keep using the
lightwalletd gRPC.
