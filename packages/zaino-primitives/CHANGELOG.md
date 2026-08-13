# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
