---
default: minor
---

# `zaino-source`: The JSON-RPC implementation moves in (absorbing `zaino-rpc` and `zaino-source-zebra-rpc`): …

The JSON-RPC implementation moves in (absorbing `zaino-rpc` and `zaino-source-zebra-rpc`): `ZebraRpcAdapter::at(address, cookie_path, user, password, timeouts, limits)` builds an adapter without probing the validator, and `on(Lane)` shares its `LinkLimits` budget per lane (`Control`, `Serve`, `Sync`). `RpcClient` batches calls, re-sending only work-queue-full items, and times out on silence (`Timeouts`), not total duration. Blocks and transactions decode once from consensus bytes (`decode_transaction`, `prepare_transaction`). `TrafficBalancer::failover` asks validators cheapest first (power of two choices over peak-EWMA load) until one answers. `IndexerWatch` follows zebrad's indexer tip and mempool push streams. Per-validator metrics via `describe_metrics`. Feature `testing`: `mock::MockChain` serves real `testing::Chain` blocks, plus captured mainnet fixtures (`fixture_block`, `fixture_transactions`).
