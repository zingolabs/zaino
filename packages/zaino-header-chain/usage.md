# `zaino-header-chain` — usage

The Validated Header tree: each header validated either by the trusted validator that served it
or by Zaino's `validate`. Anchored at a trusted validator's tip − `anchor_below()` (no history from
genesis): the bounded tree of branches above the final boundary, the tip with the most cumulative
work, and the immutable `VerifiedChain` everything downstream reads. A trusted header enters on
its parent link and its nBits work alone (`insert`: its validator already ran the consensus
rules); a peer's runs every rule first (`insert_untrusted`). Why and how it is driven:
[architecture §4](../../docs/architecture.md#4-the-best-chain).

```rust,ignore
use zaino_header_chain::{decode_header, HeaderChain, Inserted};

let mut chain = HeaderChain::new(depth);              // in memory, nothing held
chain.anchor(&decode_header(&raw)?, claim_height - chain.anchor_below()); // max(depth, CONTEXT)

for header in &run {                                  // a trusted validator's headers, in order
    match chain.insert(header)? {                     // parent link + work, best, bounds
        Inserted::Best { reorg } => { /* new most-work tip */ }
        Inserted::Side | Inserted::Known => {}
    }
}
chain.vouch(last_of_the_run);                         // a trusted validator's best chain had it
if let Some(boundary) = chain.finalizable() {         // min(highest vouched, best − depth)
    chain.finalize(boundary);
}
if let Some(boundary) = chain.peer_finalizable() {    // best − depth, no difficulty collapse
    if enough_netgroups_carry(boundary) {             // the driver's half (zaino-chainview)
        chain.finalize_by_peers(boundary);
    }
}
let published = chain.verified();                     // Option<VerifiedChain> (None = no anchor)
let ceiling = chain.ceiling(batch);                   // fetch no higher (bounds the tree)
```

## Insert (trusted)

`HeaderChain::insert(&header)` refuses only what breaks the tree, each by name (`Rejected`):
`Orphan` (parent unknown: no orphan pool), `BelowFinal` (parent final but off the final chain),
`Bits` (nBits not a valid compact target: no work to count), `WorkOverflow`. Work is counted from
the anchor (work 0). `anchor(header, height)` drops everything held and makes `header` the final
tip: a start, or a jump to a trusted validator far ahead.

## `validate` (untrusted fallback)

`validate(params, header, height, ancestors, now)` runs every consensus rule: alone (version ≥ 4
as int32, nBits valid, solution size, hash ≤ target, Equihash (200, 9); proof of work off on
regtest) then against up to `CONTEXT` = 113 ancestors (median time past, + 90 min, nBits =
zcashd's `CalculateNextWorkRequired` + ZIP 218's window by height, testnet's minimum-difficulty
gap) and the clock (`FromTheFuture`: deferred, never blamed, `is_deferred`).
`insert_untrusted` runs it before `insert`; `zaino-chainview` feeds it peers' headers. On regtest
nBits is fixed at the limit and the solution must be 36 bytes.

A solution length must be a minimal compactSize (`decode_header` refuses `fd 24 00` for 36 bytes
as `NonMinimalLength`, as zcashd's `ReadCompactSize`).

## The tree, the best tip, finality

- Every branch above the final tip is kept, within bounds (H4): at most `4 · depth` side-branch
  nodes and 32 side-branch tips; past either, the lowest-work side leaf goes (last received on a
  tie). A driver fetching nothing above `ceiling(batch)` keeps the best branch within
  `depth + batch` of the final tip (H9).
- The best tip is the leaf with the most cumulative work; a tie keeps the first received (H1).
- Finality (H6): `vouch(block)` records that a trusted validator once had `block` on its best
  chain (it and every ancestor, for good). `finalizable()` names the best-chain block at
  `min(highest vouched, best − depth)`; `finalize(block)` moves every best-chain header up to it
  into the finals and prunes every branch not descending from it. Finalizing a block off the best
  branch, shallower than `depth` (H2) or above the highest vouched (H6) is a caller bug: it
  panics naming the invariant.
- Finality on peers' word: with no vouch that deep, `peer_finalizable()` names best − depth
  unless an unfinal best-chain block holds under half the work of the highest vouched block (or
  the anchor): a difficulty collapse, the mark of a minority miner's private chain. The caller
  adds its own evidence (zainod: live peers in more than 5 netgroups carrying the chain) and calls
  `finalize_by_peers(block)`, which panics on any other block. Peer-finalized blocks never move
  the difficulty reference. `best_height_of(hash)` places a hash on the best branch.
- Memory only: the newest max(`2 · depth`, `CONTEXT`) final headers stay, answering by height; older finals are
  final by definition (`on_best` = true) and a header whose parent is one of them = `Orphan`.

## `VerifiedChain`

`HeaderChain::verified()` is an immutable snapshot (clone = refcounts: `imbl` throughout). A
holder's answers never change (H5).

| Method | Answer |
| --- | --- |
| `best()` | the most-work tip |
| `final_tip()` | the last final block (always set: the anchor at least) |
| `hash_at(h)` / `header_at(h)` | the best chain above the final tip and the finals kept; `None` above best or below the finals kept |
| `on_best(at)` | `at` on the best chain: a held header's hash, or at/below the final tip past the finals kept |
| `holds(at)` | `on_best(at)`, or a side block held above the final tip (the NFS prunes by it) |
| `locator()` | zcashd's `GetLocator`, ending at the final tip |
| `forks()` / `branch(tip)` | one `Fork` per side leaf (≤ 32, most work first) / its side blocks above `from` |

## Invariants, checked

`HeaderChain::check()` asserts H1, H2 (nothing before an anchor, finals contiguous and at most
max(`2 · depth`, `CONTEXT`), every node descends from the final tip), H4, H5, H6 and the tree's bookkeeping
(cumulative work, child counts, leaf set), naming the invariant in its panic.

## Tests

- Captured headers (`examples/capture_headers.rs`): mainnet 0–300, 653,500–653,700 (Blossom),
  3,508,500–3,508,800; testnet 299,000–299,400 and 583,800–584,200. Every header passes
  `validate`, every nBits reproduced; each mutation of a real header refused by its own rule; a
  chain anchored mid-mainnet follows 200 trusted headers with no history below.
- Model (`random_header_trees_answer_like_the_naive_model`): random trusted trees with any nBits
  (orphans, side sprays past the bounds, vouches, trusted runs, finalizations) against a naive
  tree; `check()` after every insert.
- Fire drills: each check in `check()` and each precondition, seen firing on a planted bug.
- Builder agreement (`testing`): random `MockChain` shapes, `MockChain::tip()` =
  `HeaderChain::best()` after each.
- For consumers' tests (`testing` feature), `testing::HeaderViews` on a `MockChain`:

  ```rust,ignore
  use zaino_header_chain::testing::{insert, HeaderViews};

  let mut headers = chain.header_chain(depth);         // anchored at genesis
  insert(&mut headers, &chain.blocks(tip))?;           // a trusted run: its last vouched
  let verified = chain.verified(tip);                  // final = genesis
  let pinned = chain.verified_final(tip, h(9));        // final through 9 (depth = tip − 9)
  ```

Heavy run required after changes: see [docs/testing.md](../../docs/testing.md#heavy-run).
