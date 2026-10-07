# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent
of how they are transported or stored. Every other Zaino crate that needs a
height, hash, block or amount depends on this one.

## Dependencies: `thiserror`, `zcash_protocol` and `sha2` only

Anything added here lands in every crate above it. `zcash_protocol` is there
for `NetworkType`, which every crate above already speaks; `sha2` for
`MerkleRoot::of_txids`, the consensus merkle root a block's transactions must
rebuild (the producer's body check, and the chain builder's headers). In
particular there is no serde: formats are owned by the boundary that speaks
them.

| Direction | Owner |
|---|---|
| validator JSON-RPC reply → domain | `zaino-source` (`parse.rs` for JSON, `decode.rs` for consensus bytes) |
| domain → disk | each index crate's `Persistent*` records (`zaino-persistence`) |
| domain → gRPC | `zaino-grpc` and the index crates, onto `zaino-proto` types |

Fields may carry Zcash protocol bytes (raw blocks, transactions, serialized
trees): those are consensus-defined encodings, so they are domain facts. JSON
values never belong here.

## Modules

```rust
use zaino_primitives::network::{chain_name, network_name};
use zaino_primitives::protocol::{MAX_BLOCK_BYTES, MAX_BLOCK_REORG_HEIGHT};
use zaino_primitives::types::{Block, BlockHash, Height, TransactionId, Treestate};
use zaino_primitives::types::rpc::{BlockDeltas, MiningInfo, NodeInfo, PeerInfo};
```

- `types` — the chain: `Block` (the one decoded block every index
  consumes, off `zaino_sync::BlockSink`), `BlockHeader`, `ChainMetadata` (cumulative tree sizes, derived by
  the compact-block index), `Transaction` (and `types::transaction` parts,
  Sprout's value balance included; `TransparentData::coinbase` marks the
  coinbase), `Fee` (`Coinbase`, or `Paid` = what the transaction leaves in the
  transparent pool) and `BlockFees` (one `Fee` per transaction, named by block
  hash), `OutPoint`
  (`txid` + `vout`: a transparent input is the outpoint it spends, and the key
  both transparent indexes store under),
  `BlockHash`, `TransactionId`, `Height`, `BlockRef`, `TreeSize`, `TreeRoot`,
  `Treestate`, `SubtreeRoot`, `ShieldedPool`, `BlockchainInfo` and the
  network-upgrade types, plus the zatoshi family and `CompactDifficulty` below.
- `types::rpc` — domain answers to `zaino-source` ports for validator queries
  (`BlockDeltas`, `BlockHeaderVerbose`, `BlockSubsidy`, `ChainTip`,
  `MiningInfo`, `NodeInfo`, `PeerInfo`, `SpentInfo`, `TxOut`, …). Only named,
  typed fields; `Option` means "the validator may not report it".
- `network` — `NetworkType`'s two spellings: `chain_name` (`main` / `test` /
  `regtest`: lightwalletd's `chainName` and the tree state's `network`) and
  `network_name` (`mainnet` / `testnet` / `regtest`: zainod's config, logs and
  messages). Every surface uses one of these, never its own match.
- `protocol` — `MAX_BLOCK_REORG_HEIGHT` (1000) and `MAX_BLOCK_BYTES`
  (2,000,000, the spec's `MAX_BLOCK_SIZE`). Stated as protocol facts, not
  borrowed from a node. Restating them elsewhere is a bug.

## Invariants live in constructors

```rust
let h = Height::try_from(800_000u32)?;                // ≤ 2^31 - 1
let z = Zatoshis::new(21_000_000)?;                   // ≤ money supply
let b = Block::try_new(header, txs)?;                 // non-empty tx list
let c = CompactCiphertext::try_new(&bytes)?;          // exactly 52 bytes
```

- `Height::checked_add` / `checked_sub` are checked, never wrapping.
- `Transaction` stores no index: position is list order, and
  `Block::coinbase()` is transaction 0.
- `CompactCiphertext` is the 52-byte compact head of a note ciphertext;
  once built it converts infallibly to `[u8; 52]`.
- `TreeSize::checked_add` enforces the compact protocol's `u32` range
  (`TreeSizeOutOfRange`); `TreeSizes::advance(&block)` = the cumulative sizes
  after a block (one commitment per Sapling output, Orchard or Ironwood action).

## Zatoshi family

| Type | Range (both ends inclusive) | Is |
|---|---|---|
| `Zatoshis` | `0` to `supply` | an amount: balance, UTXO value, one movement |
| `ZatoshisFlowSum` | `0` to `u128::MAX` | a sum of movements (not supply-bounded) |
| `SignedZatoshis` | `-supply` to `supply` | a signed movement or difference |

```rust
let received = ZatoshisFlowSum::try_accumulate(outputs.iter().copied())?; // None only past u128::MAX
let lifetime = ZatoshisFlowSum::from_summed(total_u64);                    // source-summed total
let net: Option<SignedZatoshis> = received.net(spent);                     // None if incoherent
let total: Option<Zatoshis> = Zatoshis::sum_balances(balances.iter().copied()); // coexisting balances; None past supply
let parsed = SignedZatoshis::new(value_i64)?;                          // boundary input
```

Movements recount the same coins, so their sum is its own type; coexisting
balances cannot exceed the supply, so `sum_balances` lands back in `Zatoshis`
and a total past the supply means double-counted input.

## Difficulty

`CompactDifficulty` is a header's nBits once validated. `try_from_bits(u32)`
applies a validator's acceptance rules (sign bit clear, target within 256 bits,
non-zero) plus the target's work fitting 128 bits, and reports each rejection
as its own `CompactDifficultyError` variant. The work
(`floor(2^256 / (target + 1))`) is computed for that check only and never
exposed. Zaino holds no chain-work type and does no work-based fork choice:
the tip is agreement by hash across the configured validators, until the
header chain's most-work rule replaces it
([chainview §2](../../docs/design/chainview.md#2-the-best-chain-proof-of-work)).

## Byte order

`BlockHash` and `TransactionId` hold internal (hash-output) byte order and
convert via `From<[u8; 32]>` both ways. Their `Display` renders the reversed
(RPC/explorer) hex; any other display-order rendering happens at the boundary
that presents it.

## Features

`testing` exposes the one chain builder every test below the live suite uses. Enable it
from `[dev-dependencies]` only.

```rust,ignore
use zaino_primitives::testing::{encode_header, Chain};

let mut chain = Chain::new();                        // regtest genesis, bare coinbase
let tip = chain.extend(chain.genesis().hash, 10);    // ten bare blocks on genesis
let fork = chain.mine(chain.path(tip.hash)[8].header().hash); // a branch at any height
let paid = chain.mine_with(tip.hash, transactions);  // exactly these transactions
let early = chain.mine_at(tip.hash, time);           // a chosen header time
let heavy = chain.mine_heavier(parent, &replaced);   // one block outweighing `replaced` (< 256)
let blocks: Vec<Block> = chain.path(tip.hash);        // genesis ..= tip, linked
let raw = encode_header(blocks[3].header());         // the consensus header bytes
```

Every block is real: `hash` = SHA-256d of `encode_header` (`header_hash`), `prev_hash`
links, `merkle_root` = the Bitcoin merkle tree over its txids (`MerkleRoot::of_txids`, an
odd level's last duplicated; a repeated pair has no root, CVE-2012-2459), version 4, regtest nBits (`0x200f0f0f`), a zero 36-byte
solution, each block 75 s after its parent. A mint counter sets each nonce and default
coinbase txid, so siblings never collide and every run builds the same hashes.
`Chain::with_genesis(transactions)` starts from a genesis holding chosen transactions;
`linked(per_block)` builds one such branch whole, as the `Arc<Block>`s an index sink takes.
`encode_header` is pinned against five mainnet headers (`zaino-source` decode tests) and
the genesis bytes here; `zaino-header-chain`'s model inserts builder headers through
every regtest rule.
