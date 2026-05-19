# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-05-19

This release accumulates main-branch development since the previous
stable tag `v0.1.2-zr4` (April 2025). Many JSON-RPC passthrough
methods were added to `JsonRpSeeConnector`, the error type system was
restructured, and `CompactBlock.proto_version` reporting was corrected.

### Added

#### JSON-RPC passthrough methods on `JsonRpSeeConnector`

Each method below ships with at least one response type and (where
relevant) a typed error under `jsonrpsee::response`.

- `get_block_count` → `GetBlockCountResponse` (#340)
- `get_difficulty` → `GetDifficultyResponse` (#343)
- `validate_address` → `ValidateAddressResponse` (#369)
- `get_best_blockhash` → `GetBlockHash` (#371)
- `z_validate_address` → `ZValidateAddressResponse`,
  `KnownZValidateAddress`, `ValidZValidateAddress`,
  `InvalidZValidateAddress`, `ZValidateAddressType`,
  `ZValidateAddressError`, and the `DEPRECATION_NOTICE` constant.
  Ships pre-deprecated; offered solely for zcashd `z_validateaddress`
  bugwards compatibility (#389)
- `get_block_deltas` → `BlockDeltas`, `BlockDelta`, `Spend`,
  `OutputDelta`, `InputDelta`, `BlockDeltasError` (#399)
- `get_mempool_info` → `GetMempoolInfoResponse` (#418)
- `get_network_sol_ps` → `GetNetworkSolPsResponse` (#573)
- `get_mining_info` → `GetMiningInfoWire`, `MiningInfo` (#585)
- `get_block_subsidy` → `GetBlockSubsidy`, `BlockSubsidy`,
  `FundingStream` (#592)
- `get_peer_info` → `GetPeerInfo`, `ZcashdPeerInfo`, `ZebradPeerInfo`,
  `PeerStateStats` (shipped alongside `get_block_subsidy` in #592)
- `get_block_header` → `GetBlockHeader`, `VerboseBlockHeader`,
  `FullBlockHeader`, `GetBlockHeaderError` (#603)
- `get_address_deltas` → `GetAddressDeltasResponse`, `AddressDelta`,
  `GetAddressDeltasParams`, `GetAddressDeltasError` (#632)
- `get_tx_out` → `GetTxOutResponse` (#1085)
- `get_chain_tips` → `GetChainTipsResponse`, `ChainTip`,
  `ChainTipStatus` (#1092)
- `get_spent_info` → `GetSpentInfoResponse`, `GetSpentInfoRequest`,
  `GetSpentInfoError` (#1093)

#### New shared types

- Numeric / time primitives: `Zatoshis`, `ZATS_PER_ZEC`, `ZecAmount`,
  `Bytes`, `SecondsF64`, `UnixTime`, `TimeOffsetSeconds`,
  `BlockHeight`, `MaybeHeight`, `LockBoxStream`.
- Block-level: `BlockObject`, `BlockInfo`, `ChainWork`,
  `ChainWorkError`.
- Transaction-level: `TxIn`, `TxOut`, `Output`, `Spend`, `Script`,
  `Solution`.
- Address/UTXO: `AddressData`.
- Wire / RPC plumbing: `RpcError`, `JsonRpcError`, `ErrorsTimestamp`,
  `RpcRequestError<E>`, the `ResponseToError` trait, `TransportError`,
  `ProtocolVersion`, `NodeId`, `ServiceFlags`, `CommonFields`,
  `RawMempoolResponse`.
- Per-method error types covering both new and pre-existing RPCs:
  `GetBlockError`, `GetUtxosError`, `GetBalanceError`,
  `GetSubtreesError`, `GetTreestateError`, `SendTransactionError`,
  `TxidsError`.

### Changed

- **Breaking — error system rewrite.** `JsonRpSeeConnectorError` is
  removed. Every method on `JsonRpSeeConnector` now returns a typed
  `Result`:
  - Setup methods (`new_with_basic_auth`, `new_with_cookie_auth`,
    `new_from_config_parts`, `uri`) return
    `Result<_, TransportError>`.
  - RPC methods return `Result<_, RpcRequestError<E>>` where `E` is
    method-specific (e.g. `GetBlockError`, `GetUtxosError`,
    `TxidsError`) or `Infallible` when the method has no
    application-level error semantics.

  Existing call sites that matched on `JsonRpSeeConnectorError` must
  be rewritten against the new typed errors.
- **Breaking — `JsonRpSeeConnector::new_from_config_parts` parameter
  list.** The constructor now takes the validator RPC address as a
  `&str` alongside its other parameters; the previous
  `validator_cookie_auth: bool` toggle has been replaced.
- **Breaking — `JsonRpSeeConnector::get_tree_state` return shape.**
  `GetTreestateResponse.sapling` and `.orchard` are now `Option<…>`.
  In regtest mode these fields may be omitted when the corresponding
  network upgrade activation height is not configured.

### Removed

- `JsonRpSeeConnectorError` — replaced by `RpcRequestError<E>` and
  `TransportError` (see the error-system entry above).
- `SerializedTransaction` from the public surface.

### Fixed

- `CompactBlock.proto_version` returned by `GetBlockRange` now reports
  `1` to match `lightwalletd`. Previously reported `4`, which was
  incompatible with clients of the lightwallet protocol; corrected to
  `0` in #1059 and finalized to `1` in #1064 (tracking #1058).
- Zallet regtest interoperability fixes for the indexer service
  (#1000, tracking [#943]).
