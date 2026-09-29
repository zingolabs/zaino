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

## [0.3.0] - 2026-09-26
### Added
- Add RelativeChainWork, the proof-of-work total a run of blocks holds, measured from where the run begins rather than from genesis.
- `types::TransparentAddressKey` — how an index keys a transparent output: a 20-byte hash and the script form it came from. Shared vocabulary, because both halves of an address's history key by it and a consumer merging them must not have to translate. `from_script` shares `classify_script`, so a store indexing an output and a chain head reporting on one cannot classify it differently.
### Changed
- Zatoshi quantities are three checked types: `Zatoshis` (an amount, `0 ..= supply`), `ZatoshisFlowSum` (an accumulation of movements, bounded only by `u128`), and `SignedZatoshis` (a movement or a net, `-supply ..= supply`). `accumulate`, `sum_balances` and `ZatoshisFlowSum::net` are the only arithmetic, and `AddressBalance.received` is a `ZatoshisFlowSum`. (#1504, #1505)
  _Migration:_ `SignedZatoshis::new(i64)` is removed: parse a value with `SignedZatoshis::try_new`, or derive one with `ZatoshisFlowSum::net`. Read `AddressBalance.received` as a `ZatoshisFlowSum`, since lifetime receipts can exceed the supply.
- The `Confirmations = i64` alias is replaced by `BlockConfirmations` (`NotInBestChain` or `Confirmed(NonZeroU32)`) and `TxConfirmations` (`Mempool` or `Mined(BlockConfirmations)`). Each type converts to and from the RPC integer with `to_rpc_i64` and `try_from_rpc_i64`.
  _Migration:_ Replace sign checks on the integer with the enum variants or `is_in_best_chain`. Build a best-chain block's value with `BlockConfirmations::of_best_chain_block(height, tip)`.
- `TransparentAddress` is validated at construction: `try_new` accepts only P2PKH and P2SH addresses of any Zcash network and rejects other kinds with a typed `TransparentAddressError`. `network()` returns the new `AddressNetwork`, and `script_type()` returns the address's `ScriptType`.
  _Migration:_ Replace `TransparentAddress::new` with `TransparentAddress::try_new` and handle `TransparentAddressError`.
- Chain work is two types: `AbsoluteChainWork`, the cumulative work from genesis, and `SingleBlockWork`, one block's contribution, with `genesis`, `accumulate` and `rollback` operations. Each work error is defined in the module that raises it.
  _Migration:_ Replace the single chain-work type with `AbsoluteChainWork` for totals and `SingleBlockWork` for one block's work.
- `TreeSize` is a checked newtype backed by `u32`. `TreeRootInfo`, `BlockTreeSizes` and `ChainMetadata` use it in place of `u64` and `u32` fields, and an oversized value is refused instead of truncated. `ChainMetadata::ZERO` and `ChainMetadata::new` are added. (#549)
  _Migration:_ Build tree sizes with `TreeSize::from(u32)` or its checked constructor, and read them through `TreeSize`.
- `CompactDifficulty` is validated at construction and computes work natively (nBits to target to work). A valid but very small target reports a typed `WorkOverWidth` instead of panicking.
  _Migration:_ Construct difficulty values through `CompactDifficulty`'s checked constructor and handle `CompactDifficultyError`.
- `EncryptedCiphertext` is renamed `CompactCiphertext`, because it holds only the 52-byte compact prefix. It is a `[u8; 52]` behind an exact-length `try_new`.
  _Migration:_ Rename `EncryptedCiphertext` to `CompactCiphertext`, and construct it with `try_new` from exactly 52 bytes.
- SingleBlockWork::new takes a NonZeroU128 and is infallible; SingleBlockWork::try_new and ZeroWork are removed.
  _Migration:_ Prove the value non-zero before the call: SingleBlockWork::new(NonZeroU128::new(work).expect(...)) or, for a literal, a const evaluated at compile time. Handle a zero where the integer is produced; the constructor no longer reports it.
- AbsoluteChainWork::try_from_reported and ChainWorkOverWidth are removed; no validator Zaino reads reports chainwork, so no wire value converts into the type. to_be_bytes and a new from_be_bytes, with ChainWorkBytesError, are the one definition of the 32-byte form.
  _Migration:_ Build an AbsoluteChainWork from a NonZeroU128 through new, or fold it from SingleBlockWork values. Read 32 stored bytes with from_be_bytes, which refuses an over-width or all-zero value; there is no absence case.

## [0.2.1] - 2026-09-11

### Added
- `BlockTxPosition`: a transaction's position as a block height and an
  index within the block, with `is_coinbase` read from the position.
- `MempoolInfo`, moved here from zaino-state.
- `ScriptType` and `classify_script`, which classify a transparent output
  script into its 20-byte hash and its type.
- `ShieldedPool::ALL`, every shielded pool in activation order.
### Changed
- Documentation no longer refers to zcashd.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-08-28

### Added
- `types::EquihashSolution`, and `version` / `solution` on `BlockHeader`. A
  header now carries everything needed to re-derive its own hash. Breaking for
  code that constructs `BlockHeader` literals.
- `types::ChainStateEpoch` — a generation plus the tip it describes, naming
  *which* chain state a published view represents. Lives here because two
  subsystems need the vocabulary and neither may depend on the other: the chain
  head publishes epochs, and the mempool's coherence layer freezes and thaws
  against them. It replaces `zaino_chain_head::ChainHeadEpoch` and
  `zaino_mempool::NonFinalizedEpoch`, which were field-identical.
### Changed
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-14

### Added
- New crate. Zaino's domain vocabulary: chain types (`Block`, `BlockHeader`,
  `Transaction`, `BlockHash`, `TransactionHash`, `Height`, `TreeRoot`,
  `Treestate`, `ShieldedPool`, `ChainMetadata`, `Zatoshis`, `SignedZatoshis`)
  and, under `types::rpc`, the passthrough response shapes in domain
  vocabulary rather than any interface's (`BlockDeltas`, `BlockchainInfo`,
  `ChainTip`, `MempoolInfo`, `MiningInfo`, `NodeInfo`, `PeerInfo`, `SpentInfo`,
  `TxOut`, `TxOutSetInfo`, `BlockSubsidy`, `AddressBalance`, `AddressDelta`,
  `Utxo`, `SubtreeRoot`).
- Ironwood (NU6.3) throughout: `ChainMetadata::ironwood_tree_size`,
  `Treestate::ironwood`, `TreeRoots::ironwood`, `ShieldedPool::Ironwood`.
  Uniformly `Option` per pool, with defaulting applied at the conversion
  boundary rather than fabricated here.
- `PoolTreestate::final_root`, so `z_gettreestate` can serve `finalRoot`
  without the domain having to omit it.
- `BlockRef` (`types::BlockRef`) — a block named by hash and height, with
  `from_tip` / `From<(BlockHash, Height)>`. Chain-wide vocabulary rather than
  any one subsystem's: a response echoing back the range it covered and a
  mempool set tagged with the tip it was read at are the same question.

### Changed
- **Dependency policy** — this crate depends on `thiserror` and nothing else,
  and that is a constraint rather than a description. In particular there is no
  serde: a derive here would let the wire format and the domain model decide
  each other. Serialization belongs to whichever boundary owns the format —
  see `usage.md` and ADR-0009.
- Byte order is internal throughout. `BlockHash` and `TransactionHash` hold
  bytes in protocol order, not display order; the reversal happens at the
  boundary that presents.
- `BlockRef` moved from `types::rpc` (where it was defined inside
  `address_deltas`) to the top-level `types`, and gained `Hash`. It was never
  an RPC-response shape — `getaddressdeltas` was just the first caller — and
  the mempool subsystem had independently defined an identical copy. One
  canonical type, one path to it.

### Deprecated
### Removed
### Fixed
