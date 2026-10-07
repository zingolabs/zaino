# MockChain: one chain builder for every test

Status: **design** (2026-10-07). Every in-repo test gets blocks, headers, a verified chain and a
validator from one builder, `zaino_primitives::testing::MockChain`, and the views its owning crates
put on it. Every block it hands out passes our own checks; nothing else builds a block.

## 1. Inventory (today)

| Location                                                                                                                                                                                              | Builds                                                                                                                                                                                                                                                                                                                                                                 | Users                                                  | Overlap                                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| `zaino-primitives::testing` (`Chain`, `linked`, `encode_header`, `header_hash`)                                                                                                                       | regtest block tree, real header bytes, SHA-256d hash, merkle root, linkage, +75 s; `mine_bits` (any nBits), `mine_heavier` (exponent − 1 = 256× work, `None` past `u128`); bare coinbase from a mint counter                                                                                                                                                           | 40 files in 13 crates                                  | the base; everything below re-wraps it                                                                   |
| `zaino-index-compact-block::testing` (`block`, `chain`, `committed`)                                                                                                                                  | `block(h)`: an h-block chain rebuilt per call, one fixed tx in every block (also in genesis), fee 5 000 stated; unknown prevout `[0x22;32]:7`                                                                                                                                                                                                                          | compact-block `serve`/`build`, grpc `routes/blocks.rs` | hides the tx the serve tests assert on; O(h²) rebuild                                                    |
| `zaino-source::mock` (`MockChain`, `fixture_block`, `fixture_transactions`)                                                                                                                           | `ChainDataSource` over held `Block`s: best chain by height and hash, `extend_best`/`rewind_to`, mempool at a flat `MEMPOOL_FEE`, `fail_next`, `set_reachable`; mined tx body = its txid (a lie); no latency, empty upgrade schedule                                                                                                                                    | chainview, nfs, grpc, zainod                           | name clashes with the builder; `FakeValidator` wraps it for what it lacks                                |
| `zaino-chainview/src/tests.rs` (`FakeValidator`, `FakePeers`, `transaction`)                                                                                                                          | mempool listing with per-tx fee, metadata failures, relay verdicts, reorg between tip read and `getblockhash`, call counters; scripted p2p (`ValidatorP2pSource`); real empty v4 tx bytes                                                                                                                                                                              | chainview only                                         | second validator double; `transaction` duplicated in `zaino-peers` tests                                 |
| `zaino-chainview/src/network_model.rs` (`Sim`, `Node`)                                                                                                                                                | N simulated zebrads: mine, relay (longest wins), fork, unreachable                                                                                                                                                                                                                                                                                                     | chainview model                                        | hand-rolled network world over `Chain` + `MockChain`                                                     |
| `zaino-header-chain` (`regtest_in_memory`, `insert_blocks`, `VerifiedChain::regtest`, `Params::with_genesis`, `any_bits`)                                                                             | header chain over a builder genesis on `SimFs`, any nBits; `VerifiedChain` of a path, nothing final                                                                                                                                                                                                                                                                    | chainview, nfs, grpc, zainod (32 call sites)           | genesis + params passed by hand; finality set by hand per test                                           |
| per-crate tx helpers                                                                                                                                                                                  | `txs(seed, sapling, orchard, ironwood)` (compact-block `fold`, `writer`), `pools` (tree-state `writer`), `tx`/`p2pkh`/`txid`/`zat` (transparent-address `fold`, `writer`, `serve`), `tx`/`coinbase`/`fees` (value-balance `fold`, shared to its writer), `one_output` (grpc `tree_state`), `transactions(funding, tag)` (nfs `tests`), `coinbase(h)` (zainod `verify`) | their own crate                                        | 35 `Transaction { .. }` literals in 17 files; p2pkh bytes spelled in 10 files; canonical-leaf trick in 4 |
| `zaino-nfs` `core/model.rs`, `core/fire_drills.rs`                                                                                                                                                    | lying sources in-model (`WrongBlock`, `Poisoned`, `Mutated`, `Slow`, `Failing`, `Silent`) via a decoy block                                                                                                                                                                                                                                                            | nfs core                                               | the lies the validator double lacks                                                                      |
| `zaino-grpc/src/testing.rs`                                                                                                                                                                           | `indexed` (fold one index from genesis), `snapshot` (activations hard-coded at genesis), `routes_over`, request framing                                                                                                                                                                                                                                                | grpc routes                                            | activations not derived from the chain                                                                   |
| zainod `indexer.rs`, `serving.rs`, `verify.rs`                                                                                                                                                        | pipeline over `MockChain` + `HeaderChain`; `mine_bits(.., 0x1f0f_0f0f)` by hand                                                                                                                                                                                                                                                                                        | zainod                                                 | heavier branch spelled as raw nBits                                                                      |
| header capture (`header-chain/tests/fixtures/*.headers`, `examples/capture_headers.rs`); block capture (`source/tests/fixtures/block_*.hex`, `examples/capture_fixtures.rs`, `tests/block_parity.rs`) | real mainnet/testnet bytes                                                                                                                                                                                                                                                                                                                                             | header-chain rules, source decode, grpc send tests     | none: real data, stays                                                                                   |
| `zaino-source` `rpc/client.rs`, `indexer.rs` (`FakeIndexer`), `zaino-peers` tests                                                                                                                     | JSON-RPC wire fake, zebrad indexer gRPC push streams, scripted zebra-network peer                                                                                                                                                                                                                                                                                      | their crate                                            | wire-level; stays (§5 decision 4)                                                                        |
| `persistence::conformance::block_ref`; `live-tests/` (`zaino-testutils`)                                                                                                                              | `BlockRef` from an integer; nothing (live validators = oracle)                                                                                                                                                                                                                                                                                                         | persistence; live suite (separate workspace)           | none; both stay, out of scope                                                                            |

## 2. The builder

### Placement

The core builder lives in `zaino-primitives` behind its `testing` feature. `zaino-header-chain` and
`zaino-source` run unit tests on the builder's blocks. A separate `zaino-testing` crate would depend
on them and be a dev-dependency of them. Cargo allows that cycle, but the crate under test then links
two copies of itself, so `zaino_testing::verified(..)` returns a `VerifiedChain` that is not
`crate::VerifiedChain`. Each view therefore lives in the crate that owns its output type, behind
that crate's existing `testing` feature:

| View                                    | Crate (`testing` feature) | Shape                              |
| --------------------------------------- | ------------------------- | ---------------------------------- |
| blocks, headers, fees, `BlockchainInfo` | `zaino-primitives`        | inherent methods on `MockChain`    |
| `HeaderChain`, `VerifiedChain`          | `zaino-header-chain`      | `trait HeaderViews` on `MockChain` |
| validator double (`ChainDataSource`)    | `zaino-source`            | `MockValidator`                    |
| p2p double (`ValidatorP2pSource`)       | `zaino-chainview`         | `MockPeers`                        |
| `ChainParams`                           | `zaino-nfs`               | `ChainParams::of(&MockChain, tip)` |

### Core API (`zaino_primitives::testing`)

```rust
pub struct MockChain { /* blocks: HashMap<BlockHash, Mined>, genesis, upgrades, work, network, minted */ }
pub enum Work { Limit, Varied }
pub struct Upgrades(BTreeMap<NetworkUpgrade, Height>); // zcash_protocol::consensus::NetworkUpgrade
impl Upgrades {
    pub fn all_at(height: Height) -> Self; // Overwinter ..= Nu6_3 (Nu7 off)
    pub fn with(self, upgrade: NetworkUpgrade, at: Height) -> Self;
    pub fn without(self, upgrade: NetworkUpgrade) -> Self;
}
impl MockChain {
    pub fn regtest() -> Self; // Upgrades::all_at(1), Work::Limit, NetworkType::Regtest
    pub fn upgrades(self, upgrades: Upgrades) -> Self; // before the first mine
    pub fn varied_work(self) -> Self; // Work::Varied: outweigh() allowed
    pub fn network(self, network: NetworkType) -> Self; // label only: schemas, addresses, params
    pub fn genesis_with(self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> Self;
    pub fn genesis(&self) -> BlockRef;
    pub fn tip(&self) -> BlockRef; // most work, first mined on a tie
    pub fn at(&self, height: Height) -> BlockRef; // on the best chain
    pub fn mine(&mut self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> BlockRef;
    pub fn mine_empty(&mut self, count: u32) -> BlockRef; // bare coinbases on tip()
    pub fn branch(&mut self, parent: BlockRef) -> Branch<'_>; // any held block
    pub fn fork(&mut self, at: Height) -> Branch<'_>; // = branch(at(at))
    pub fn block(&self, hash: BlockHash) -> &Arc<Block>;
    pub fn blocks(&self, tip: BlockRef) -> Vec<Arc<Block>>; // genesis ..= tip
    pub fn header_bytes(&self, hash: BlockHash) -> Vec<u8>; // consensus encoding
    pub fn fees(&self, hash: BlockHash) -> BlockFees;
    pub fn blockchain_info(&self, tip: BlockRef) -> BlockchainInfo; // getblockchaininfo at tip
    pub fn schedule(&self) -> (&Upgrades, Work, NetworkType); // what views derive params from
}
pub struct Branch<'c> { /* chain, tip, outweigh: bool */ }
impl Branch<'_> {
    pub fn outweigh(self) -> Self; // next block: just enough work to beat best above the fork
    pub fn mine(self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> Self;
    pub fn mine_empty(self, count: u32) -> Self;
    pub fn tip(&self) -> BlockRef;
}
impl BlockBuilder {
    pub fn coinbase(self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self; // default: bare
    pub fn tx(self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self; // in order after coinbase
    pub fn time(self, unix: u32) -> Self; // header rule tests only
    pub fn bits(self, bits: u32) -> Self; // header rule tests only; Work::Varied
}
impl TxBuilder {
    pub fn txid(self, txid: [u8; 32]) -> Self; // default: SHA-256d(mint counter)
    pub fn spend(self, prevout: OutPoint) -> Self;
    pub fn pay(self, script: Script, zats: u64) -> Self;
    pub fn sapling_spend(self, nullifier: [u8; 32]) -> Self;
    pub fn sapling_output(self, leaf: u32) -> Self; // cmu = leaf LE (canonical, both moduli)
    pub fn orchard_action(self, nullifier: [u8; 32], leaf: u32) -> Self;
    pub fn ironwood_action(self, nullifier: [u8; 32], leaf: u32) -> Self;
    pub fn value_balance(self, pool: ShieldedPool, zats: i64) -> Self;
    pub fn sprout_balance(self, zats: i64) -> Self;
    pub fn fee(self, zats: u64) -> Self; // asserted against conservation, not trusted
}
pub fn p2pkh(hash: [u8; 20]) -> Script;
pub fn outpoint(txid: [u8; 32], vout: u32) -> OutPoint;
```

- `u64`/`i64` zats at the call site (literals stay short); out-of-supply = panic naming the tx
- `fee` optional: unstated → derived from conservation; `fees(hash)` = `[Coinbase, Paid(..)..]`

### Headers, nBits, Equihash

- Header = version 4, `prev_hash`, merkle root of the txids, zero commitments, time, nBits, nonce
  from the mint counter, 36-byte zero solution; hash = SHA-256d of `encode_header` (today's)
- Rules = `Params::regtest`: proof of work off (no Equihash, no hash ≤ target); solution length,
  linkage, median-time, nBits and work enforced. Mainnet/testnet PoW headers cannot be mined: the
  captured fixtures stay the real-data layer
- Time = parent + spacing at that height from `upgrades` (150 s pre-Blossom, 75 s, 25 s from NU7)
- `Work::Limit`: every nBits = regtest limit, so the header chain checks nBits strictly
  (`Difficulty::Limit`); heavier = longer only
- `Work::Varied`: `outweigh()` picks the lowest-work nBits whose work > (best's work above the fork
  − branch's so far); the header-chain view switches to `any_bits()` only for a varied chain.
  Minimal margin = cumulative work grows linearly per reorg (today: 256× per nesting, `None` after
  ~14 nested reorgs)

### Branches, reorgs, finality

| Shape                   | Call                                                                       |
| ----------------------- | -------------------------------------------------------------------------- |
| extend                  | `chain.mine_empty(3)` / `chain.mine(\|b\| ..)`                             |
| longer fork             | `chain.fork(h(9)).mine_empty(4)` (Limit suffices)                          |
| same-height replacement | `chain.fork(h(12)).outweigh().mine_empty(1)`                               |
| retreat                 | `chain.fork(h(10)).outweigh().mine_empty(1)` (best was 12)                 |
| revive an old tip       | `chain.branch(old).outweigh().mine_empty(1)`                               |
| side branch, not best   | `chain.branch(parent).mine_empty(2)`                                       |
| finality                | header chain's, never the builder's: `chain.verified_final(tip, final_at)` |

### Views

```rust
// zaino_header_chain::testing
pub trait HeaderViews {
    fn header_chain(&self, depth: ReorgDepth) -> HeaderChain; // SimFs, genesis inserted
    fn verified(&self, tip: BlockRef) -> VerifiedChain; // nothing final
    fn verified_final(&self, tip: BlockRef, final_at: Height) -> VerifiedChain;
}
impl HeaderViews for MockChain { /* Params from genesis, upgrades (Blossom, NU7), work */ }
pub fn insert(chain: &mut HeaderChain, blocks: &[Arc<Block>]) -> Result<(), Rejected>;
// zaino_source::testing
pub struct MockValidator { /* best: Vec<Arc<Block>>, mempool, script: Script, calls */ }
impl MockValidator {
    pub fn following(chain: &MockChain, tip: BlockRef) -> Self;
    pub fn follow(&self, chain: &MockChain, tip: BlockRef); // reorg/retreat = another tip
    pub fn reorg_after_next_poll(&self, chain: &MockChain, tip: BlockRef);
    pub fn estimate(&self, height: Height); // getblockchaininfo estimatedheight
    pub fn mempool_insert(&self, raw: Vec<u8>, fee: u64); // txid from prepare_transaction
    pub fn relay(&self, verdict: Result<(), SendRawTransactionError>);
    pub fn metadata(&self, peers: Option<Vec<PeerInfo>>, release: Option<NodeRelease>); // None = times out
    pub fn latency(&self, per_call: Duration); // tokio::time (paused clock)
    pub fn fail_next(&self, count: u32, mode: FailureMode);
    pub fn reachable(&self, reachable: bool);
    pub fn lie(&self, lie: Option<Lie>);
    pub fn calls(&self) -> Calls; // polls, links, blocks, sends
}
pub enum Lie { WrongBlock, Poisoned, Mutated, WrongHeight }
pub fn raw_transaction(lock_time: u32, expiry: u32) -> (TransactionId, Vec<u8>);
pub mod fixtures { pub fn block(height: u32) -> Vec<u8>; pub fn transactions(height: u32) -> Vec<Vec<u8>>; }
// zaino_chainview::testing
pub struct MockPeers { /* live, dead, pushes, announce */ }
```

- Answers as zebrad does: best chain only by height and hash, nothing above the tip, upgrades from
  `chain.blockchain_info(tip)`, a mined txid leaves the mempool
- One per simulated node over one shared `MockChain`: the chainview network model and
  verified-chain.md §10's simulation = N `follow` calls

### Values under test stay visible

The rule (no setup helpers that hide the values under test) splits by who reads the value:

- Builder-owned = never asserted: hashes, nonces, times, nBits, merkle roots, default txids,
  ephemeral keys and ciphertexts (deterministic, from the mint counter); read back only through
  the builder (`chain.at(h)`)
- Test-owned = asserted: every output value, script, spend, commitment leaf, nullifier, fee and
  any txid a test compares. Written at the call site through `TxBuilder`, one line each
- Banned: per-crate `txs(seed, ..)`/`block(h)` wrappers that return transactions a test asserts on
  (`compact-block::testing::block` today). `TxBuilder` makes the literal short enough to inline

### Example

```rust
/// Block 2 spends block 1's 50 000 coinbase, paying 49 000 (fee 1 000) + one sapling output +
/// one orchard action: its record = the block encoded with that fee and sizes 1 / 1 / 0
#[test]
fn a_spend_carries_its_fee_and_commitments_into_the_compact_block_record() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(alice.clone(), 50_000)));
    let spent = chain.mine(|b| {
        b.tx(|t| {
            t.txid([0x20; 32])
                .spend(outpoint([0x10; 32], 0))
                .pay(alice.clone(), 49_000)
                .fee(1_000)
                .sapling_output(7)
                .orchard_action([0x04; 32], 9)
        })
    });
    let paid = Fee::Paid(Zatoshis::new(1_000).expect("in supply"));
    assert_eq!(chain.fees(spent.hash).fees, [Fee::Coinbase, paid]);

    let open = DiskEngine::new(SimFs::new()).open(Path::new("/cb"), &schema(REGTEST));
    let mut store = open.expect("open");
    let mut last = Vec::new();
    for block in chain.blocks(spent) {
        let parent = CompactBlockReader::new(store.staged(), REGTEST);
        let changes = fold(&parent, &block, &chain.fees(block.header().hash)).expect("small");
        last = changes.appends(BLOCKS).map(<[u8]>::to_vec).collect();
        store.apply(changes);
    }
    let sizes = TreeSizes { sapling: 1.into(), orchard: 1.into(), ironwood: 0.into() };
    let block = chain.block(spent.hash);
    assert_eq!(last, [encode_compact_block(block, &chain.fees(spent.hash), &sizes)]);
}
```

## 3. Migration

| Today                                                                                  | Becomes                                                                                         |
| -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `testing::Chain`, `linked`, `mine_with`, `mine_at`, `extend`, `path`                   | `MockChain` (`mine`, `mine_empty`, `blocks`); deleted                                           |
| `mine_bits`, `mine_heavier`                                                            | `BlockBuilder::bits` (header model only), `Branch::outweigh`; deleted                           |
| `encode_header`, `header_hash`                                                         | kept (`header_bytes` wraps them)                                                                |
| compact-block `testing` (`block`, `chain`, `committed`)                                | deleted; callers inline `TxBuilder` + fold loop                                                 |
| `zaino_source::mock::MockChain`                                                        | `zaino_source::testing::MockValidator`                                                          |
| chainview `FakeValidator`, `FakePeers`, `transaction`                                  | `MockValidator` (absorbed), `chainview::testing::MockPeers`, `source::testing::raw_transaction` |
| `regtest_in_memory`, `insert_blocks`, `VerifiedChain::regtest`, `Params::with_genesis` | `HeaderViews`, `header_chain::testing::insert`; deleted                                         |
| per-crate `tx`/`txs`/`pools`/`one_output`/`transactions`/`coinbase`/`p2pkh`            | inline `TxBuilder`; deleted                                                                     |
| grpc `snapshot` activations at genesis                                                 | `ChainParams::of(&chain, tip)` (via `PoolActivations::from_validator`)                          |
| nfs model `Kind` lies + decoy                                                          | `Lie` shapes shared with `MockValidator` (model stays sans-IO)                                  |
| `fixture_block`, `fixture_transactions`                                                | `zaino_source::testing::fixtures`                                                               |

Order (dependency order; each step = `cargo nextest run --workspace` + clippy green, old API
deleted in the step that removes its last caller):

1. `zaino-primitives`: add `MockChain`, `Upgrades`, `Branch`, `BlockBuilder`, `TxBuilder`, builder
   tests; migrate `block.rs`, `per_pool.rs` tests
1. `zaino-header-chain`: `HeaderViews`, `insert`, cross-check test (§4); migrate `tests.rs`,
   `model.rs`, `chain/fire_drills.rs`
1. `zaino-source`: `MockValidator` (absorbing `FakeValidator`'s features), `raw_transaction`,
   `fixtures`; migrate `decode.rs` and the mock's own test
1. `zaino-sync` (`committer`), then value-balance, block-hash, transparent-address, tree-state,
   compact-block (deletes its `testing` module once grpc's `blocks.rs` is moved in step 7)
1. `zaino-chainview`: `MockPeers`; migrate `tests.rs`, `network_model.rs`, `holders/`
1. `zaino-nfs`: `ChainParams::of`; migrate `tests.rs`, `fold.rs`, `fetch.rs`, `core/model.rs`,
   `core/fire_drills.rs`
1. `zaino-grpc`: `testing.rs` keeps request framing + `routes_over`; routes and `tests/serve.rs`
1. `zainod`: `indexer.rs`, `serving.rs`, `verify.rs`; then delete `Chain` and every helper left in
   the table; update `docs/testing.md`, `verified-chain.md` §10, each touched crate's `usage.md`
   (`zaino-persistence` untouched throughout; any change there = the heavy proptest loop)

## 4. Invariants (every block handed out) and the builder's own tests

|     | Invariant                                                                                                                  | Checked by                                    |
| --- | -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| M1  | hash = SHA-256d(`encode_header`); genesis bytes golden                                                                     | primitives test                               |
| M2  | `prev_hash` = parent hash, height = parent + 1, time > parent's median-time-past                                           | primitives + M8                               |
| M3  | merkle root = `MerkleRoot::of_txids`; txids distinct chain-wide (an explicit repeat = panic)                               | primitives + `check_block`                    |
| M4  | exactly one coinbase, slot 0 (BIP 34 height: no script sig in `Transaction`, no zaino check reads it)                      | primitives                                    |
| M5  | `Work::Limit` = limit nBits; `outweigh()` = best right after its first block, minimal margin                               | primitives (work sums)                        |
| M6  | each spend = an unspent output on its own branch (ancestor or earlier in the block); value conserved, stated fee = derived | primitives (panic table) + value-balance fold |
| M7  | pool data only from its upgrade (Sapling, NU5 = orchard, NU6.3 = ironwood); leaves canonical                               | primitives + tree-state fold                  |
| M8  | every path inserts into `header_chain(..)` and `chain.tip()` = the header chain's best                                     | header-chain proptest                         |
| M9  | `MockValidator` serves only its best chain, by height and hash; a `Lie` never passes `check_block`                         | source + nfs tests                            |

Builder tests, few and dense:

- `primitives`: one golden scenario (genesis bytes, linkage, siblings distinct, fees, `blocks`), one
  table of rejected constructions (unknown prevout, double spend across a block, overspend, wrong
  stated fee, orchard before NU5, `outweigh` under `Work::Limit`)
- `header-chain`: proptest over random shapes (extend, fork, outweigh, revive, side branch): after
  each, every new block inserts and `mock.tip()` = `chain.best()` (the builder's best and the real
  header chain agree, each the other's oracle)
- `source`: today's `serves_its_best_chain_..` test extended with latency, lies and counters

## 5. Open decisions

1. **Placement.** Recommended: core in `zaino-primitives`, views in owning crates (above). A
   `zaino-testing` crate on top only works if header-chain, source and chainview move their unit
   tests to `tests/`, losing `pub(crate)` access (`rules::expected_bits`, fire drills).
1. **Default schedule.** Recommended: every upgrade through NU6.3 at height 1, genesis = bare
   coinbase (as zebrad regtest; real genesis has no shielded data). Tests that fold pools at genesis
   today move them to height 1; their golden sizes shift by one block.
1. **Mined transaction bytes.** `MockValidator::get_transaction` today answers a mined tx with its
   txid as the body. Recommended: `BlockBuilder::raw_tx(Transaction, Vec<u8>)`, the pair from
   `zaino_source::testing::decoded(bytes)` (decode lives above primitives), keeps real bytes; a
   synthetic tx asked for by `get_transaction` panics naming the tx, never invents bytes.
1. **Trait-level, not JSON wire.** Recommended: `MockValidator` implements `ChainDataSource`.
   Consensus bytes for shielded blocks need valid curve points per action; wire decoding stays
   pinned by captured fixtures, `block_parity.rs` and `rpc/client.rs`. Revisit if a wire-level
   double is needed for the traffic balancer.
1. **Varied work opt-in.** Recommended: `Work::Limit` default (strict nBits), `varied_work()`
   declared by every reorg test, so the relaxed `any_bits()` rule never applies silently.
1. **Fold-from-genesis oracle.** grpc `indexed`, nfs `own_fold` and each writer's `folded`
   duplicate one loop. Not chain building; a follow-up (`zaino-nfs::testing::fold_from_genesis`).
