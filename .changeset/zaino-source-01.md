---
default: major
---

# `zaino-source`: One port per trusted validator replaces the per-question trait catalogue: `ChainDataSource` …

One port per trusted validator replaces the per-question trait catalogue: `ChainDataSource` (`get_block_by_hash`, `get_block_links`, `get_poll_reading`, `get_raw_mempool_transactions`, `get_transaction`, `send_raw_transaction`) with one domain error per question (`GetBlockByHashError`, `GetBlockError`, `GetMempoolListingError`, `GetRawMempoolTransactionError`, `GetTransactionError`, `SendRawTransactionError`) inside `QueryError<E>`, whose transport side is `NonDomainError`. Every `Get*` / `OneShot*` trait, `SourceLifecycle`, `ValidatorClient`, `RetryPolicy`, `PolledChainTip`, `SubscribeBlocks`, `SubscribeChainTip`, `TipObservation`, `FetchError`, `SourceError`, `UnavailableError` and `MempoolTxMeta` are removed, as is the `zaino-source-macros` dependency.

**Migration:** Implement `ChainDataSource` where you implemented the per-question traits (a test fake answers what its test asks and `unimplemented!()`s the rest) and bound on `S: ChainDataSource`. Read the tip, mempool listing, peers and release from `get_poll_reading`.
