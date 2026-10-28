# Legacy surface → this stack

Every RPC and index dev's legacy stack served (`origin/dev`: zaino-serve, zaino-state, zaino-chain-store-zainodb, zaino-chain-head, zaino-mempool), and its state after the migration.
The one deliberate status table in docs (owner's request): kept until no row is `Not built yet`, then deleted.

States: `Migrated` = same capability served · `Migrated (gRPC only)` = JSON-RPC form gone, gRPC equivalent exists · `Not built yet` = wanted, needs work · `Dropped` = intentionally gone · `Proxy` = dev only forwarded to the validator.

## gRPC (+ admin HTTP)

Routes = `packages/zaino-grpc/src/routes/`; dispatch = `Method` in `packages/zaino-grpc/src/service.rs`.

| Legacy | State | This stack | Notes |
| --- | --- | --- | --- |
| `GetLatestBlock` | Migrated | `blocks.rs`; served tip | dev: chain-head best tip |
| `GetBlock` | Migrated | `blocks.rs`; compact-block (+ block-hash by hash) | dev: store / chain head, validator filled holes |
| `GetBlockNullifiers` | Dropped | — (`UNIMPLEMENTED`) | not in vendored proto; `GetBlock` / `GetBlockRange` `poolTypes` |
| `GetBlockRange` | Migrated | `blocks.rs`; compact-block | ends `OUT_OF_RANGE` past the index tip |
| `GetBlockRangeNullifiers` | Migrated | `deprecated_nullifiers.rs`; compact-block | deprecated alias (pepper-sync); TODO: REMOVE |
| `GetTransaction` | Migrated | `transactions.rs`; validator → served mempool → txid-location + peer block | `(block, index)` arm `INVALID_ARGUMENT` (dev: same) |
| `SendTransaction` | Migrated | `transactions.rs`; chain view submission | dev: validator `sendrawtransaction`; now `checkadmission` + peer relay |
| `GetTaddressTransactions` | Migrated | `transparent_address.rs`; transparent-address + `GetTransaction` path | dev: validator `getaddresstxids` |
| `GetTaddressTxids` | Migrated | `transparent_address.rs` (= `GetTaddressTransactions`) | deprecated alias (pepper-sync); TODO: REMOVE |
| `GetTaddressBalance` | Migrated | `transparent_address.rs`; transparent-address | dev: validator `getaddressbalance` |
| `GetTaddressBalanceStream` | Migrated | `transparent_address.rs`; transparent-address | dev: validator `getaddressbalance` |
| `GetMempoolTx` | Migrated | `mempool.rs`; chain view mempool | dev: mirror of one validator's mempool |
| `GetMempoolStream` | Migrated | `mempool.rs`; chain view mempool | ends when the served tip moves |
| `GetTreeState` | Migrated | `tree_state.rs`; tree-state (+ block-hash by hash) | dev: validator `z_gettreestate` + roots |
| `GetLatestTreeState` | Migrated | `tree_state.rs`; tree-state | dev: validator, at chain-head tip |
| `GetSubtreeRoots` | Migrated | `tree_state.rs`; tree-state `<pool>/subtrees` | dev: validator `z_getsubtreesbyindex` |
| `GetAddressUtxos` | Migrated | `transparent_address.rs`; transparent-address | dev: validator `getaddressutxos` |
| `GetAddressUtxosStream` | Migrated | `transparent_address.rs`; transparent-address | dev: validator `getaddressutxos` |
| `GetLightdInfo` | Migrated | `info.rs`; `GrpcSnapshot` | dev: validator `getblockchaininfo`; now no validator call |
| `Ping` (dev: `UNIMPLEMENTED` stub) | Dropped | — (`UNIMPLEMENTED`) | testing-only; not in vendored proto |
| `zaino.index.v1.IndexedTipService/SubscribeIndexedTips` | Dropped | — (`UNIMPLEMENTED`) | poll `GetLatestBlock` |
| admin `/metrics` | Migrated | `packages/zainod/src/health/admin.rs` | + protobuf exposition by `Accept` |
| admin `/livez` | Migrated | `packages/zainod/src/health/admin.rs` | — |

- Not on dev: `GetStandardFee` (`info.rs`), admin `/readyz`, gRPC-listener `/readyz` + `/status` (`health.rs`)

## JSON-RPC

Dev: `ZcashIndexerRpc` (28 methods, `packages/zaino-serve/src/rpc/jsonrpc/service.rs`), port 8237 + cookie auth. This stack: no JSON-RPC server.

| Legacy | State | This stack | Notes |
| --- | --- | --- | --- |
| `getinfo` | Proxy | — | validator's own |
| `getblockchaininfo` | Proxy | — | validator's own; tip / branch / upgrade in `GetLightdInfo` |
| `getmempoolinfo` | Dropped | — | dev: own mempool mirror size; validator's own (mempool = consensus) |
| `getmininginfo` | Proxy | — | validator's own |
| `gettxoutsetinfo` | Not built yet | — | dev: own accumulator; utxo-set `outputs` = value only, no aggregate |
| `getbestblockhash` | Migrated (gRPC only) | `GetLatestBlock.hash` | dev: chain-head best tip |
| `getdifficulty` | Proxy | — | validator's own |
| `getblockdeltas` | Not built yet | — | dev: validator (zcashd) or zebra ReadState; needs spent prevouts' address + value |
| `getpeerinfo` | Proxy | — | validator's own |
| `getblocksubsidy` | Proxy | — | validator's own |
| `getblockcount` | Migrated (gRPC only) | `GetLatestBlock.height` | dev: chain-head best tip |
| `getchaintips` | Dropped | — | dev: chain-head graph; validator's chain view; header chain keeps branches unserved |
| `validateaddress` | Dropped | — | dev: local `zaino_address` parse; validator's own (zebra serves it) |
| `z_validateaddress` | Dropped | — | deprecated; as `validateaddress` |
| `getaddressbalance` | Migrated (gRPC only) | `GetTaddressBalance`; transparent-address | dev: proxied to validator |
| `sendrawtransaction` | Migrated (gRPC only) | `SendTransaction`; chain view submission | dev: proxied to validator |
| `getblock` | Proxy | — | dev: validator raw + verbose; `GetBlock` = compact form only |
| `getblockheader` | Proxy | — | validator's own |
| `getrawmempool` | Migrated (gRPC only) | `GetMempoolTx` (`CompactTx.txid`) | dev: own mempool mirror |
| `z_gettreestate` | Migrated (gRPC only) | `GetTreeState`; tree-state | dev: proxied to validator |
| `z_getsubtreesbyindex` | Migrated (gRPC only) | `GetSubtreeRoots`; tree-state | dev: proxied to validator |
| `getrawtransaction` | Migrated (gRPC only) | `GetTransaction` (bytes + height) | verbose object → validator's own |
| `gettxout` | Proxy | — | consensus UTXO; utxo-set lacks script / height / coinbase flag |
| `getspentinfo` | Not built yet | — | dev: validator (zcashd `-spentindex`); transparent-address `spent` lacks vin index |
| `getaddresstxids` | Migrated (gRPC only) | `GetTaddressTransactions`; transparent-address | full txs, height range; dev: proxied |
| `getaddressutxos` | Migrated (gRPC only) | `GetAddressUtxos`; transparent-address | dev: proxied to validator |
| `getaddressdeltas` | Not built yet | — | `receives` + `spent` ≈ deltas; missing spend vin index |
| `getnetworksolps` | Proxy | — | validator's own |

- zebra-rpc 16.0.0 serves none of `gettxoutsetinfo`, `getblockdeltas`, `getspentinfo`, `getaddressdeltas`, `getchaintips`

## Indexes / stores

Dev LMDB = `zaino-chain-store-zainodb` schema v1.3 (`db_schema_v1.txt`). Ours = `zaino-persistence` `DiskEngine` per index.

| Legacy | State | This stack | Notes |
| --- | --- | --- | --- |
| LMDB `headers` (hash, parent, version, time, bits, nonce) | Migrated | header chain (memory); compact-block record header; tree-state `heights` | hash + time persisted; headers re-fetched, never stored whole |
| LMDB `headers.chain_work` (absolute U256) | Dropped | — | header chain counts work from its anchor; no RPC serves absolute chainwork |
| LMDB `txids` | Migrated | compact-block `blocks` (`CompactTx.txid`) | — |
| LMDB `transparent` | Migrated | compact-block `blocks` (`vin` / `vout`) | — |
| LMDB `sapling` / `orchard` / `ironwood` | Migrated | compact-block `blocks` | every pool stored, pruned on read |
| LMDB `commitment_tree_data` (roots + sizes) | Migrated | tree-state `heights` + `<pool>/lNN`; compact-block `chainMetadata` | + frontiers + subtree roots (dev: validator) |
| LMDB `heights` (hash → height) | Migrated | block-hash-to-height `by_hash` | — |
| LMDB `spent` (outpoint → `Vec<TxLocation>`) | Migrated | transparent-address `spent` (outpoint → height + spender txid) | no vin index |
| LMDB `tx_out_set_info_accumulator` | Not built yet | — | utxo-set `outputs` has no count / total / hash |
| LMDB `address_history` (non-default feature) | Migrated | transparent-address `receives` + `spent` | dev served address RPCs from validator regardless |
| LMDB `metadata` (version, schema hash, migration status) | Migrated | `DiskEngine` `MANIFEST` + per-index `schema(network)` | no migration status (formats stay 1) |
| LMDB `txid_location` | Migrated | txid-location `by_txid` (+ block hash) | — |
| ephemeral store (validator passthrough, DB off) | Dropped | — | no fallbacks: disabled index = `UNIMPLEMENTED` |
| schema migrations v1.0 → v1.3 | Dropped | — | no prod operators; formats stay 1 |
| chain-head graph (non-finalised window, branches, tx locations, spenders, txout delta) | Migrated | header chain (branches) + `zaino-nfs` layers per index | — |
| mempool mirror + coherence (`zaino-mempool-service`) | Migrated | `zaino-chainview` mempool view | N trusted validators + peer quorum |
| `zaino-chain` composer gap fill (store↔head hole from validator) | Dropped | — | derived answers never from a validator (architecture §1) |

- Not on dev: utxo-set `outputs` + `fees` (`CompactTx.fee`), tree-state retained nodes + `subtrees`

## Not built yet

- `gettxoutsetinfo`: utxo-set aggregate (output count, tx count, total zat, multiset hash) per fold; dev's hash also covers script, which `outputs` lacks
- `getspentinfo`: vin index in transparent-address `spent` rows + a serving surface
- `getaddressdeltas`: same vin index; `blockindex` from txid-location; + a serving surface
- `getblockdeltas`: spent prevouts' address + value (utxo-set deletes them) + header fields; + a serving surface
- Every row above: no JSON-RPC server, so each needs a surface (gRPC method or other) first
