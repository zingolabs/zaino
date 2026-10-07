# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent
of how they are transported or stored. Every other Zaino crate that needs a
height, hash, block or amount depends on this one.

## Dependencies: `thiserror`, `zcash_protocol` and `sha2` only

Anything added here lands in every crate above it. `zcash_protocol` is there
for `NetworkType`, which every crate above already speaks; `sha2` for
`sha256d`: block hashes (`HeaderBytes::hash`) and `MerkleRoot::of_txids`, the
consensus merkle root a block's transactions must rebuild (the NFS's body
check, and the chain builder's headers). In particular there is no serde:
formats are owned by the boundary that speaks them.

| Direction | Owner |
|---|---|
| validator JSON-RPC reply → domain | `zaino-source` (`parse.rs` for JSON, `decode.rs` for consensus bytes) |
| consensus header bytes → fields | `HeaderBytes` here (shared by `zaino-source` and `zaino-header-chain`) |
| domain → disk | each index crate's own `encode` / `decode` (`zaino-persistence` sees bytes only) |
| domain → gRPC | `zaino-grpc` and the index crates, onto `zaino-proto` types |

Fields may carry Zcash protocol bytes (raw blocks, transactions, serialized
trees): those are consensus-defined encodings, so they are domain facts. JSON
values never belong here.

## Modules

```rust
use zaino_primitives::network::{chain_name, network_name};
use zaino_primitives::protocol::{MAX_BLOCK_BYTES, MAX_BLOCK_REORG_HEIGHT};
use zaino_primitives::sha256d;
use zaino_primitives::types::{Block, BlockHash, HeaderBytes, Height, TransactionId, Treestate};
```

- `types` — the chain: `Block` (the one decoded block every index consumes,
  inside each `zaino_sync::Final`; `at()` = its `BlockRef`), `BlockHeader`
  (`extends(parent: Option<BlockRef>)` = one height above `parent` and linked to
  it by `prev_hash`, genesis on `None`: every fold's precondition), `Transaction` and its parts
  (`TransparentData`, `SaplingData`, `OrchardData`, `SproutData`, …; Sprout's
  value balance included; `TransparentData::coinbase` marks the coinbase), `Fee`
  (`Coinbase`, or `Paid` = what the transaction leaves in the transparent pool)
  and `BlockFees` (one `Fee` per transaction, named by block hash), `OutPoint`
  (`txid` + `vout`: a transparent input is the outpoint it spends, and the key
  both transparent indexes store under), `BlockHash`, `TransactionId`, `Height`,
  `BlockRef`, `ReorgDepth`, `TreeSize` / `TreeSizes` (`PerPool<TreeSize>`),
  `TreeRoot`, `Treestate`, `SubtreeRoot`, `ShieldedPool`, `MerkleRoot`,
  `HeaderBytes`, `BlockchainInfo`, `NodeRelease`, `PeerInfo`,
  `TransactionLocation` and the network-upgrade types, plus the zatoshi family
  and `CompactDifficulty` below.
- `network` — `NetworkType`'s two spellings: `chain_name` (`main` / `test` /
  `regtest`: lightwalletd's `chainName` and the tree state's `network`) and
  `network_name` (`mainnet` / `testnet` / `regtest`: zainod's config, logs and
  messages). Every surface uses one of these, never its own match.
- `protocol` — `MAX_BLOCK_REORG_HEIGHT` (1000) and `MAX_BLOCK_BYTES`
  (2,000,000, the spec's `MAX_BLOCK_SIZE`). Stated as protocol facts, not
  borrowed from a node. Restating them elsewhere is a bug.
- `sha256d` — SHA-256 twice, the one implementation (block hashes, merkle
  nodes, pre-v5 txids).

## Invariants live in constructors

```rust
let h = Height::try_from(800_000u32)?;                // ≤ 2^31 - 1 (also from u64)
let z = Zatoshis::new(21_000_000)?;                   // ≤ money supply
let c = CompactCiphertext::prefix_of(&note);          // the 52-byte head of a note ciphertext
let (header, rest) = HeaderBytes::split(&raw)?;       // one consensus header + what follows
```

- `Height` converts from `u32` and `u64` (`HeightOverflow` names the rejected
  value); `checked_add` / `checked_sub` are checked, never wrapping.
- `Transaction` stores no index: position is list order, and transaction 0 is
  the coinbase.
- `TreeSize` is `u32`-backed (`TryFrom<u64>` refuses a full depth-32 tree,
  `TreeSizeOutOfRange`); `TreeSizes::advance(&block)` = the cumulative sizes
  after a block (one commitment per Sapling output, Orchard or Ironwood action).
- `HeaderBytes::split` checks the layout (fixed fields present, solution 1344
  or regtest's 36 bytes behind a minimal compactsize; `HeaderError` names each
  fault) and reads every field from the bytes; its `hash()` is SHA-256d of
  those bytes, never a field taken on trust.

## Zatoshi family

| Type | Range (both ends inclusive) | Is |
|---|---|---|
| `Zatoshis` | `0` to `supply` | an amount: balance, UTXO value, one movement |
| `SignedZatoshis` | `-supply` to `supply` | a signed movement or difference (a value balance) |

```rust
let total: Option<Zatoshis> = Zatoshis::sum_balances(balances.iter().copied()); // None past supply
let parsed = SignedZatoshis::new(value_i64)?;                                  // boundary input
```

Coexisting balances cannot exceed the supply, so `sum_balances` lands back in
`Zatoshis` and a total past the supply means double-counted input.

## Difficulty

`CompactDifficulty` is a header's nBits once validated. `try_from_bits(u32)`
applies a validator's acceptance rules (sign bit clear, target within 256 bits,
non-zero) plus the target's work fitting 128 bits, and reports each rejection
as its own `CompactDifficultyError` variant. The work
(`floor(2^256 / (target + 1))`) is computed for that check only and never
exposed: chain work and the most-work rule live in `zaino-header-chain`
([chainview §2](../../docs/design/chainview.md#2-the-best-chain-proof-of-work)).

## Byte order

`BlockHash` and `TransactionId` hold internal (hash-output) byte order and
convert via `From<[u8; 32]>` both ways. `Display` renders the reversed
(RPC/explorer) hex and `FromStr` parses it back (`ParseHashError`: not 64 hex
digits); that pair is the one display-order conversion.

## Features

`testing` exposes `MockChain`, the one chain builder every test below the live suite
uses (`docs/design/mock-chain.md`). Enable it from `[dev-dependencies]` only.

```rust,ignore
use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain, Upgrades};

let alice = p2pkh([0xaa; 20]);
let mut chain = MockChain::regtest();                // every upgrade through NU6.3 at 1, bare genesis
chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
let spent = chain.mine(|b| {
    b.tx(|t| {
        t.spend(outpoint([0x10; 32], 0))
            .pay(&alice, 49_000)
            .fee(1_000)                              // asserted against conservation
            .sapling_output(7)                       // cmu = 7 little-endian (canonical)
            .orchard_action([0x04; 32], 9)
    })
});
let tip = chain.mine_empty(3);                       // bare blocks on the best tip
let blocks = chain.blocks(tip);                      // genesis ..= tip, as `Arc<Block>`s
let fees = chain.fees(spent.hash);                   // [Coinbase, Paid(1 000)]
let side = chain.fork(h(2)).mine_empty(1).tip();     // a side branch: not best
let info = chain.blockchain_info(tip);               // getblockchaininfo at tip

let mut varied = MockChain::regtest().varied_work(); // reorg-by-work tests opt in
let twelve = varied.mine_empty(12);
let retreat = varied.fork(h(10)).outweigh().mine_empty(1).tip(); // 12 → 11, now the best
let revived = varied.branch(twelve).outweigh().mine_empty(1).tip();
let later = MockChain::regtest().upgrades(Upgrades::all_at(h(1)).onward(NetworkUpgrade::Nu5, h(3)));
```

- Builder-owned (never asserted): hashes, nonces, times, nBits, merkle roots, default
  txids, ephemeral keys, ciphertexts. Test-owned (asserted, written at the call site):
  every value, script, spend, leaf, nullifier, fee and any txid a test compares.
- Every block passes M1–M7: hash = SHA-256d of `encode_header`; linked, one height and
  one target spacing up (150 s pre-Blossom, 75 s, 25 s from NU7), after the median time
  past; merkle root over its txids; one coinbase, slot 0, spending nothing; each spend an
  unspent output of its own branch (an ancestor, or earlier in the block), txids and
  nullifiers distinct per branch, chain value pools never negative (ZIP 209), stated fee =
  derived; pool data only once its upgrade is active. A construction breaking one panics
  naming the rule.
- `Work::Limit` (default): every nBits the regtest limit, heavier = longer. `varied_work()`:
  `outweigh()` gives the branch's next block the least work that makes it best;
  `BlockBuilder::bits` sets any nBits.
- `raw_tx(decoded(bytes))` mines a real transaction beside its bytes (`tx_bytes(txid)`);
  `TxBuilder` transactions have none, so a validator double never invents a body.
- Views live in the crates that own their types: `HeaderViews` (`zaino-header-chain`),
  `MockValidator` (`zaino-source`), `MockPeers` (`zaino-chainview`), `ChainParams::of`
  (`zaino-nfs`).

`Chain` and `linked` below remain until every test moves to `MockChain`.

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
`mine_bits` mines under a chosen nBits; `linked(per_block)` builds one such branch whole, as
the `Arc<Block>`s an index sink takes. `encode_header` is pinned against five mainnet headers
(`zaino-source` decode tests) and read back field by field through `HeaderBytes::split`;
`zaino-header-chain`'s model inserts builder headers through every regtest rule.
