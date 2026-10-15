# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent
of how they are transported or stored. Every other Zaino crate that needs a
height, hash, block or amount depends on this one.

## Dependencies: `thiserror`, `sha2`, `uint` and librustzcash's light crates only

Anything added here lands in every crate above it. `zcash_protocol`,
`zcash_transparent` and `zcash_script` carry the protocol types Zaino takes as-is
(heights, block hashes, txids, values, outpoints, scripts, upgrade schedules);
`zcash_encoding` for display-order hex (`Target::from_hex`); `sha2` for
`sha256d`: block hashes (`HeaderBytes::hash`) and `MerkleRoot::of_txids`, the
consensus merkle root a block's transactions must rebuild (the NFS's body
check, and the chain builder's headers); `uint` for the 256-bit `Target`
arithmetic (zebra's own U256). In particular there is no serde:
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
use zaino_primitives::protocol::{MAX_BLOCK_BYTES, MAX_BLOCK_REORG_HEIGHT};
use zaino_primitives::sha256d;
use zaino_primitives::types::{Block, HeaderBytes, Treestate};
```

- `types` — the chain: `Block` (the one decoded block every index consumes,
  on the finalized stream and in every NFS node; `at()` = its `BlockRef`), `DecodedHeader`
  (`extends(parent: Option<BlockRef>)` = one height above `parent` and linked to
  it by `prev_hash`, genesis on `None`: every fold's precondition), `Transaction` and its parts
  (`TransparentData`, `SaplingData`, `OrchardData`, `SproutData`, …; Sprout's
  value balance included; `TransparentData::coinbase` marks the coinbase), `Fee`
  (`Coinbase`, or `Paid` = what the transaction leaves in the transparent pool:
  `Transaction::fee(spent)`, `spent` = Σ its prevouts' values; `FeeError::Negative`
  or `Overflow`) and `BlockFees` (one `Fee` per transaction, named by block hash),
  `PeerTxId` (txid + v5 auth digest: how peers name an unmined transaction), `Announced`
  (a peer's block `inv`, its sender attached),
  `BlockRef`, `FinalityDepth`, `TreeSize` / `TreeSizes` (`PerPool<TreeSize>`),
  `TreeRoot`, `Treestate`, `SubtreeRoot`, `MerkleRoot`,
  `HeaderBytes` and `TransactionLocation`, plus `CompactDifficulty` below.
  What a validator reports about itself (`BlockchainInfo`, `PeerInfo`, …) is
  `zaino-source`'s, beside its parser.
- Upgrade schedule = `zcash_protocol`'s `LocalNetwork` (no Zaino schedule type):
  `protocol::compiled_schedule(network)` = mainnet's or testnet's (`None` for
  regtest, whose heights are set per chain); `zaino_source::BlockchainInfo::schedule()`
  = a validator's listing (regtest), keyed by branch id. Pool activations, the header
  rules (`target_spacing`, NU7's window) and `GetLightdInfo` all read it. Branch ids
  are `zcash_protocol`'s `BranchId`.
- `Block::check_body()` — its transactions are the ones its header commits to:
  `BodyMismatch::MerkleRoot` (txids rebuild another root) or `Mutated` (a repeated
  txid pair, CVE-2012-2459). With the hash matched, a body passing it is the one
  asked for, whoever sent it.
- `protocol` — `MAX_BLOCK_REORG_HEIGHT` (1000), `MAX_BLOCK_BYTES`
  (2,000,000, the spec's `MAX_BLOCK_SIZE`), `MEDIAN_TIME_SPAN` (11, the spec's
  `PoWMedianBlockSpan`) and `median_time_past(newest_first)` (upper median of
  the newest 11 times; `None` = none). Stated as protocol facts, not borrowed
  from a node. Restating them elsewhere is a bug.
- `sha256d` — SHA-256 twice, the one implementation (block hashes, merkle
  nodes, pre-v5 txids).

## Invariants live in constructors

```rust
let c = CompactCiphertext::prefix_of(&note);          // the 52-byte head of a note ciphertext
let (header, rest) = HeaderBytes::split(&raw)?;       // one consensus header + what follows
```

- Heights are librustzcash's `BlockHeight` (`H0` = genesis; `+` / `-` saturate,
  so a step that may cross genesis goes through `u32::checked_sub`).
- `Transaction` stores no index: position is list order, and transaction 0 is
  the coinbase.
- `TreeSize` is `u32`-backed (`TryFrom<u64>` refuses a full depth-32 tree,
  `TreeSizeOutOfRange`); `TreeSizes::advance(&block)` = the cumulative sizes
  after a block (one commitment per Sapling output, Orchard or Ironwood action; panics past
  2^32 leaves, which consensus never builds).
- `HeaderBytes::split` checks the layout (fixed fields present, solution 1344
  or regtest's 36 bytes behind a minimal compactsize; `HeaderError` names each
  fault) and reads every field from the bytes; its `hash()` is SHA-256d of
  those bytes, never a field taken on trust.

## Zatoshi values

Values are librustzcash's `zcash_protocol::value::Zatoshis` (an amount, `0` to `MAX_MONEY`) and
value balances its `ZatBalance` (`-MAX_MONEY` to `MAX_MONEY`). Coexisting balances cannot exceed
the supply, so their sum (`.sum::<Option<Zatoshis>>()`) is `None` only on double-counted input.

## Difficulty

`CompactDifficulty` is a header's nBits once validated. `try_from_bits(u32)`
applies a validator's acceptance rules (sign bit clear, target within 256 bits,
non-zero) plus the target's work fitting 128 bits, and reports each rejection
as its own `CompactDifficultyError` variant. A validated value gives its
`target()` and its `work()` (`floor(2^256 / (target + 1))`, always within
`u128`). This is the one nBits → target → work implementation; chain work and
the most-work rule built on it live in `zaino-header-chain`
([architecture §4](../../docs/architecture.md#4-the-best-chain)).

`Target` is the 256-bit threshold (zebra's `ExpandedDifficulty`), ordered
numerically: `is_met_by(hash)` (the hash read little-endian, ≤ target),
`to_compact()` (raw nBits, unvalidated: a target below `2^128` has no
`CompactDifficulty`), `mean(&[Target])` (overflow-free), `rescale(actual,
expected)` (`/ expected × actual`, divide first, saturating) and `from_hex` over
display-order hex (zcashd's `powLimit` literals).

## Byte order

Block hashes, txids, outpoints, scripts and zatoshi values are librustzcash's
(`zcash_protocol::BlockHash`, `zcash_protocol::TxId`,
`zcash_transparent::bundle::OutPoint`, …). Hashes hold internal (hash-output) byte
order (`.0` for a `BlockHash`'s bytes); `Display` / `as_hex` render the reversed
(RPC/explorer) hex and `from_hex` parses it back.

## Testing: `MockChain`

The `testing` feature exposes `MockChain`: the one chain builder every test below the
live suite takes its blocks, headers, fees, mempool and validators from. Enable it from
`[dev-dependencies]` only.

```rust,ignore
use zaino_primitives::testing::{all_at, h, p2pkh, Ledger, MockChain, Unmined};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::bundle::OutPoint;

let alice = p2pkh([0xaa; 20]);
let mut chain = MockChain::regtest();                // every upgrade through NU6.3 at 1, bare genesis
chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
let spent = chain.mine(|b| {
    b.tx(|t| {
        t.spend(OutPoint::new([0x10; 32], 0))
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

let mut varied = MockChain::regtest().varied_work(); // reorg-by-work tests opt in
let twelve = varied.mine_empty(12);
let retreat = varied.fork(h(10)).outweigh().mine_empty(1).tip(); // 12 → 11, now the best
let revived = varied.branch(twelve).outweigh().mine_empty(1).tip();
let no_nu7 = MockChain::regtest()                    // heights refused out of protocol order
    .upgrades(LocalNetwork { nu7: None, ..all_at(h(1)) });

let fork = varied.ancestor(revived, h(9));           // any tip's path at a height
let met = varied.fork_point(twelve, retreat);        // highest block on both paths

let txid = chain.submit(decoded(raw));               // mempool: admitted on the best, else panic
let held: &[Unmined] = chain.mempool();              // submission order, each with bytes + fee
chain.evict(txid);                                   // and every entry spending its outputs
let mined = chain.mine_mempool();                    // one block on tip() carrying all of it
let utxos: Ledger = chain.ledger(tip);               // a tip's unspent outputs, txids, nullifiers
```

- **Who writes what.** The builder owns hashes, nonces, times, nBits, merkle roots,
  default txids, ephemeral keys and ciphertexts: deterministic, never asserted on. The
  test owns every value, script, spend, leaf, nullifier, fee and any txid it compares,
  and writes each at the call site through `TxBuilder`.
- **Configure first.** `upgrades`, `varied_work`, `network` and `genesis_with` rebuild
  genesis, so they panic after the first `mine`.
- **Work.** `Work::Limit` (default): every nBits is the regtest limit, so only a longer
  branch wins. A reorg-by-work test declares `varied_work()`; then `outweigh()` gives the
  branch's next block the least work that makes it the best, and `BlockBuilder::bits`
  sets any nBits.
- **Txids and nullifiers are distinct per branch**, not chain-wide: the same transaction
  re-mined on a competing branch is the reorg shape mempool and index tests need.
- **Real transactions.** `raw_tx(decoded(bytes))` (`decoded` is in `zaino-source`'s
  `testing`) mines a real transaction and keeps its bytes (`tx_bytes(txid)`).
  `TxBuilder` transactions have no bytes, so a validator double never invents a body.
- **Mempool.** The network's unmined transactions, real bytes only. `submit` admits each on
  the best chain after every entry before it, through the same `Ledger` mining uses (unspent
  inputs, fresh nullifiers, pool activations, fee ≥ 0), and panics with the `Refused` rule
  otherwise. Whenever the best moves, the mempool is re-admitted on the new best: transactions
  only the old best mined come back first (block order; coinbases and `TxBuilder` ones never,
  having no bytes), and mined, conflicting or orphaned entries leave. `Ledger::admit` is the
  non-panicking check a validator double gives its verdicts with.

### Views in other crates

Each lives in the crate that owns its type, behind that crate's `testing` feature:

| View | Crate | Gives |
|---|---|---|
| `HeaderViews` (trait on `MockChain`) | `zaino-header-chain` | `header_params()`, `header_chain(depth)`, `verified(tip)`, `verified_finalized(tip, finalized_at)` |
| `ValidatorViews` (trait on `MockChain`) | `zaino-source` | `blockchain_info(tip)`; `validator(tip)`: a `MockValidator` (`ChainDataSource`); `follow` another tip to extend, reorg or retreat; `hear` entries of the chain's mempool; scripted latency, failures and `Lie`s |
| `PeerSetViews` (trait on `MockChain`) | `zaino-traffic` | `peer_set(tip)`: a `MockPeerSet` (`PeerTransport`) over one path, `hold(height)` to move its tip |
| `MockPeers` | `zaino-chainview` | a `PeerSource`: live peers list mempools and take pushes, dead ones refuse |
| `UnspentOutputs for Ledger` | `zaino-chainview` | `chain.ledger(tip)` as the UTXO set a peer quorum's prevouts are priced from |
| `ChainParams::of(&chain)` | `zaino-nfs` | the chain's network label and upgrade schedule |

Finality is the header chain's, never the builder's: ask for it with `verified_finalized`.

### Invariants

Every block `MockChain` hands out passes these. A construction that would break one
panics with a message naming the rule. The labels appear in test docs across the
workspace.

| | Invariant |
|---|---|
| M1 | hash = SHA-256d of `encode_header`; genesis pinned byte for byte |
| M2 | `prev_hash` = parent's hash, height = parent + 1, time after the parent's median time past (default time = parent + target spacing: 150 s pre-Blossom, 75 s, 25 s from NU7) |
| M3 | merkle root = `MerkleRoot::of_txids`; txids distinct on the branch |
| M4 | exactly one coinbase, in slot 0, spending nothing and stating no fee |
| M5 | `Work::Limit` = every nBits the regtest limit; `outweigh()` = best right after its first block, by the least work that wins |
| M6 | each spend = an output unspent on its own branch (an ancestor, or earlier in the block); nullifiers fresh on the branch; chain value pools never negative (ZIP 209); a stated fee = the derived one |
| M7 | pool data only once its upgrade is active (Sapling; Orchard from NU5; Ironwood from NU6.3) |
| M8 | every path inserts into `HeaderViews::header_chain`, and `MockChain::tip` = `HeaderChain::best` (`zaino-header-chain` proptest) |
| M9 | `MockValidator` serves its best chain only, by height and by hash; each `Lie` fails the body check (`zaino-sync` fetch tests) |

M1–M7 are enforced in `src/testing/mock_chain.rs` (the transaction rules in `ledger.rs`) and
tested in its `tests` module.

`encode_header` (`chain.header_bytes(hash)` wraps it) and `header_hash` = the consensus header
bytes and their SHA-256d: `merkle_root` = the Bitcoin merkle tree over the txids
(`MerkleRoot::of_txids`, an odd level's last duplicated; a repeated pair has no root,
CVE-2012-2459), version 4, a zero 36-byte solution. A mint counter sets each nonce and default
txid, so siblings never collide and every run builds the same hashes. `encode_header` is pinned
against five mainnet headers (`zaino-source` decode tests) and read back field by field through
`HeaderBytes::split`; `zaino-header-chain`'s model inserts builder headers through every
regtest rule.
