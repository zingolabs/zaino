# `zaino-header-chain` — usage

The best chain by proof of work (`docs/design/verified-chain.md` §3–§4, §10): every
header verified from genesis, the bounded tree of valid branches above the final
boundary, the tip with the most cumulative work, and the immutable
`VerifiedChain` everything downstream reads. Headers may come from any source;
how one arrived never changes how it is checked.

```rust,ignore
use zaino_header_chain::{check, decode_header, link_run, HeaderChain, HeaderStore, Params};

let store = HeaderStore::open(fs, &path, NetworkType::Main)?;
let mut chain = HeaderChain::open(Params::mainnet(), depth, store); // resumes from disk

// stage A: each header alone (pure: run it in parallel, off the runtime), then the run's links
let checked = headers.into_iter().map(|raw| check(&params, decode_header(&raw)?));
let (run, cut) = link_run(checked);                  // cut = the rule the run broke, if any

// stage B: in order, on the one owner
for header in &run {
    match chain.insert(header, now_unix)? {          // attach, nBits, time, work, best, bounds
        Inserted::Best { reorg } => { /* new most-work tip */ }
        Inserted::Side | Inserted::Known => {}
    }
}
if let Some(boundary) = chain.finalizable() {        // `depth` below the best
    if held_by_a_trusted_validator(boundary) {
        chain.finalize(boundary)?;                   // a store error: end the process
    }
}
let published = chain.verified();                    // Option<VerifiedChain>
```

## Rules

Stage A (`check`, the header alone) then stage B (`HeaderChain::insert`, against its
ancestors and the clock). Each refusal names its rule (`Rejected`):

| Rule | Stage | Check | Source |
| --- | --- | --- | --- |
| `Version` | A | `nVersion` read as int32 ≥ 4: the high bit set = negative = invalid; > 4 stays valid | zcashd `CBlockHeader` |
| `Bits` | A | nBits a valid compact target | zebra-chain `work::difficulty` |
| `SolutionSize` | A | 1,344 bytes (200, 9); 36 on regtest | zebra-chain `work::equihash` |
| `AboveTarget` | A | hash ≤ target (off on regtest) | zebra-chain `work::difficulty` |
| `Solution` | A | Equihash (200, 9) on the 108 bytes before the nonce (off on regtest) | zebra-chain `work::equihash` |
| `Unlinked` | A | each header's `prev_hash` = the hash before it in the run | — |
| `TimeTooEarly` | B | after the median of the previous 11 times (not genesis) | zebra-state `check.rs` |
| `TimeTooLate` | B | ≤ that median + 90 min (mainnet from height 2, testnet from 653,606) | protocol spec §7.6 |
| `Difficulty` | B | nBits = `ThresholdBits(h)` (below) | zcashd `pow.cpp`, ZIP 218 |
| `FromTheFuture` | B | ≤ the local clock + 2 h; **deferred**, never cached or blamed (`is_deferred`) | zebra-chain `time_is_valid_at` |
| `WrongGenesis`, `Orphan`, `BelowFinal`, `WorkOverflow` | B | genesis = this network's; parent held (no orphan pool); parent not off the final chain | — |

A solution length must be a minimal compactSize (`fd 24 00` for 36 bytes is
refused by `decode_header` as `NonMinimalLength`, as zcashd's `ReadCompactSize`).

**Difficulty by height.** Spacing is 150 s before Blossom, 75 s from Blossom, 25 s
from NU7; the averaging window is 17 blocks, 102 from NU7 (ZIP 218: every use of the
window takes the checked height's, so it straddles the activation). The threshold is
`rules::threshold(params, mean target, timespan, height)` — zcashd's
`CalculateNextWorkRequired` (damping 4, −16 % / +32 %, at most the limit) — with
the mean taken as Σ quotients + ⌊Σ remainders / n⌋ (102 testnet targets overflow
256 bits). Testnet's minimum-difficulty rule (from 299,188): a gap over 6 × spacing,
18 × spacing from NU7 — 450 s on both sides of NU7 (451 qualifies, 450 does not).
While the window reaches past genesis, nBits is the limit outright (zcashd; zebra
6.x mints the wrong nBits at mainnet height 1). Regtest: nBits = the limit, proof of
work off, as zebrad.

`Params` carries the NU7 height (Testnet 4,465,026; Mainnet `None` until set;
`Params::regtest(blossom, nu7)`). The rules read up to `CONTEXT` = 113 ancestors
(102 + 11).

## The tree, the best tip, finality

- Every valid branch above the final tip is kept, within bounds (H4): at most
  `4 · depth` side-branch nodes and 32 side-branch tips; past either, the
  lowest-work side leaf is evicted (the last received on a tie). The best branch is
  never evicted: it spans `depth` plus one fetch batch while a trusted validator
  holds the boundary, and grows only while none does.
- The best tip is the leaf with the most cumulative work; a tie keeps the first
  received (H1). `Inserted::Best { reorg }` says whether it extends the old one.
- An orphan (unknown parent) is refused, never pooled: the next request uses the
  locator, which reaches back to the fork.
- Finality is the caller's gate (H6): `finalizable()` names the best-chain block
  `depth` below the best tip; the caller finalizes it only when a trusted
  validator holds it (work never gates it: peers alone never finalize).
  `finalize(block)` commits every best-chain header up to it (the store first,
  memory after) and prunes every branch not descending from it. Finalizing a
  block off the best branch, or shallower than `depth`, is a caller bug: it
  panics naming H2.
- The tree is memory: a reopen resumes at the final tip, and branches above it
  come back as their headers do. The last 113 final headers stay in memory as
  context; a header whose parent is final but older than that is `Orphan`.

## `VerifiedChain`

`HeaderChain::verified()` is an immutable snapshot (clone = refcounts: an `imbl`
path above the final tip, the store's committed view below it). A holder's
answers never change (H5), whatever the chain does after.

| Method | Answer |
| --- | --- |
| `best()` | the most-work tip |
| `final_tip()` | the last final block (`None` = nothing final); never moves back |
| `hash_at(h)` / `header_at(h)` | the best chain at any `h ≤ best` (final heights from the store): hash, merkle root, time, nBits, cumulative work |
| `locator()` | zcashd's `GetLocator`: the tip, consecutive ancestors, then doubling steps, ending at the final tip (genesis while nothing is final) |
| `forks()` | one `Fork { from, tip, cumulative_work }` per side leaf (at most 32, H4), most work first; `from` = its best-chain parent, at or above the final tip |
| `branch(tip)` | the side blocks above the fork's `from` up to `tip`, ascending (empty = `tip` not a side block) |
| `holds(at)` | `at` on the best chain (final included), or a side block held above the final tip: what a consumer of side state may keep (the NFS prunes by it) |

The tree above the final tip (`nodes`, `leaves`) is `imbl` too: `verified()` shares it, so a
published chain's forks never change either.

## Invariants, checked

`HeaderChain::check()` asserts H1 (best = max-work leaf, first received), H2 (every
node descends from the final tip; final tip = the store's), H4 (bounds), H5
(`best_path` = a held chain from the final tip) and the tree's own bookkeeping
(cumulative work, child counts, leaf set), naming the invariant in its panic. It
is O(nodes): tests run it after every mutation, the driver after every run in
debug builds. Every mutating method opens with `assert!`s naming the invariant
its caller must keep (H2, H3: a `Checked` from another network's rules).

## Store

`HeaderStore` holds one 88-byte record per final height (`hash · merkle root · time
· nBits · cumulative work`, `store::encode` / `decode` beside a golden test) in one
checksummed log, committed through the `zaino-persistence` manifest as
`IndexKind::HeaderChain` (its tables: `TABLES`, layout version `FORMAT`, both
exported for `zainod verify`). It is written only with headers verified from genesis, so
a reopen never re-verifies and never starts from a checkpoint someone supplied.

## Tests

- Real chain, captured by `examples/capture_headers.rs` (`main` | `test`): mainnet
  0–300, 653,500–653,700 (Blossom), 3,508,500–3,508,800; testnet 299,000–299,400
  (minimum difficulty from 299,188) and 583,800–584,200 (Blossom). Every header
  passes stage A and B and every nBits is reproduced; the genesis range runs
  through a store and a reopen. The testnet NU7 range (4,464,900–4,465,300) needs a
  validator that activated NU7 (zebra ≥ 7) and is not captured yet.
- zcashd's `pow_tests.cpp` vectors through `threshold`; each mutation of a real
  header refused by its own rule (version high bit, non-minimal length, a pre-NU7
  nBits after NU7, the 450 / 451 s gap, linkage).
- Model (`random_header_trees_answer_like_the_naive_model`): random trees under
  any-nBits rules (work varies, most work ≠ highest) with orphan runs, side-branch
  sprays past the bounds, future headers then clock advances, finalizations and
  reopens, against a naive tree; `check()` after every insert; `forks` / `branch` /
  `holds` against each side leaf's mined ancestry.
- Fire drills: each check in `check()` and each precondition, seen firing on a
  planted bug.
- Builder agreement (`testing::the_builders_best_tip_is_the_real_header_chains_best`):
  random `MockChain` shapes (extend, fork, outweigh, revive, side branch; limit and
  varied work), every new block inserted, `MockChain::tip()` = `HeaderChain::best()`
  after each, an outweigh always the best.
- For consumers' tests (`testing` feature), `testing::HeaderViews` on a
  `zaino_primitives::testing::MockChain`: the rules its schedule implies
  (`Params::regtest` at its Blossom / NU7 heights over its genesis; any nBits only when
  the chain declared `varied_work()`), a fresh `SimFs` store, real header bytes through
  stage A and B.

  ```rust,ignore
  use zaino_header_chain::testing::{insert, HeaderViews};

  let mut headers = chain.header_chain(depth);         // genesis inserted
  insert(&mut headers, &chain.blocks(tip))?;           // a path, as header sync would
  let verified = chain.verified(tip);                  // nothing final
  let pinned = chain.verified_final(tip, h(9));        // final through 9 (depth = tip − 9)
  ```

  Still here until every caller moves: `Params::with_genesis`,
  `HeaderChain::regtest_in_memory(genesis, depth)` (regtest rules over a
  `testing::Chain`'s genesis, any nBits so `mine_bits` branches vary in work
  and the most work need not be the highest, a fresh `SimFs` store),
  `insert_blocks(path)`
  (its real headers through stage A and B, as header sync would) and
  `VerifiedChain::regtest(path)` (both at once, nothing final), so a
  `VerifiedChain` in a test is one verified, never a stand-in.

Heavy run after any change here (the in-code 256 cases are the light run):

```bash
end=$((SECONDS + 180)); round=0
while [ $SECONDS -lt $end ]; do
  round=$((round + 1))
  PROPTEST_CASES=1000 cargo test -p zaino-header-chain || { echo "FAILED in round $round"; break; }
done
```
