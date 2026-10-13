---
default: major
---

# `zaino-source`: honest validator credentials, internal JSON-RPC client

- `ZcashLegacyRpcClient::at` with neither `user` nor `password` sends no basic auth (it sent `xxxxxx:xxxxxx`); one without the other is the new `EndpointError::UnpairedCredentials`.
- `RpcClient` and `RpcClientConfig` are no longer public; build a client with `ZcashLegacyRpcClient::at`.
- Feature `testing`: `fixtures::block(height)` and `fixtures::HEIGHTS` are public.

**Migration:** configure `user` and `password` together, or neither.
