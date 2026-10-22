# `zaino-header-chain` — usage

The Validated Header tree ([architecture §4](../../docs/architecture.md#4-the-best-chain): the
anchor, trusted vs peer headers, finality): the bounded tree of branches above the finalized
boundary, the most-work tip, and the immutable `HeaderChainSnapshot` everything downstream reads. A
trusted header enters by `insert`, a peer's by `insert_untrusted` (`validate` first).

```rust,ignore
use zaino_header_chain::{decode_header, HeaderChain, Inserted, Witness};

let mut chain = HeaderChain::new(depth);              // in memory, nothing held
chain.anchor(&decode_header(&raw)?, claim_height - chain.anchor_below()); // max(depth, CONTEXT)

for header in &run {                                  // a trusted validator's headers, in order
    match chain.insert(header)? {                     // parent link + work, best, bounds
        Inserted::Best { reorg } => { /* new most-work tip */ }
        Inserted::Side | Inserted::Known => {}
    }
}
chain.witness(last_of_the_run, Witness::Trusted);     // a trusted validator's best chain had it
chain.witness(announced, Witness::Peer(netgroup));    // a live outbound peer's `inv`
if let Some(boundary) = chain.finalizable() {         // min(highest settled, best − depth)
    chain.finalize(boundary);
}
let published = chain.snapshot();                     // Option<HeaderChainSnapshot> (None = no anchor)
let stale = held_revision != chain.revision();        // any change since a snapshot was taken
let ceiling = chain.ceiling(batch);                   // fetch no higher (bounds the tree)
```

## Insert (trusted)

`HeaderChain::insert(&header)` refuses only what breaks the tree, each by name (`Rejected`):
`Orphan` (parent unknown: no orphan pool), `BelowFinalized` (parent finalized but off the finalized chain),
`Bits` (nBits not a valid compact target: no work to count), `WorkOverflow`. Work is counted from
the anchor (work 0). `anchor(header, height)` drops everything held and makes `header` the finalized
tip: a start, or a jump to a trusted validator far ahead.

## `validate` (untrusted fallback)

`validate(params, header, height, ancestors, now)` runs every consensus rule: alone (version ≥ 4
as int32, nBits valid, solution size, hash ≤ target, Equihash (200, 9); proof of work off on
regtest) then against up to `CONTEXT` = 113 ancestors (median time past, + 90 min, nBits =
zcashd's `CalculateNextWorkRequired` + ZIP 218's window by height, testnet's minimum-difficulty
gap) and the clock (`FromTheFuture`: deferred, never blamed). `Rejected::fault` names whose
refusal it is: `Sender` (blamed), `Deferred` or `Unplaced` (too little held to judge it).
`insert_untrusted` runs it before `insert`; `zaino-chainview` feeds it peers' headers. On regtest
nBits is fixed at the limit and the solution must be 36 bytes.

`Params::for_network(network, &schedule)` picks the rules: `mainnet()` / `testnet()` read the
compiled schedule (`compiled_schedule`), `regtest(schedule)` its validator's (only NU7 matters
there: it widens the averaging window). Spacing comes from `zcash_protocol`'s `target_spacing`;
nBits → target → work from
`zaino_primitives::types::CompactDifficulty`.

A solution length must be a minimal compactSize (`decode_header` refuses `fd 24 00` for 36 bytes
as `NonMinimalLength`, as zcashd's `ReadCompactSize`).

## The tree, the best tip, finality

- Every branch above the finalized tip is kept within bounds (H4); past either bound, the
  lowest-work side leaf goes (last received on a tie). `ceiling(batch)` bounds the best branch
  (H9).
- Witnesses (H7): `witness(hash, by)` records that `by` once had the block on its best chain: it
  and every ancestor, for good, a no-op unless held above the finalized tip. A block is
  **vouched** once any trusted validator witnessed it, **peer-quorate** once peers in more than
  `ECLIPSE_NETGROUPS` (5) netgroups did (only the first 6 netgroups are kept: quorate is final).
- Finality (H6): a block is **settled** when vouched, or peer-quorate while no unvouched
  best-chain block holds under half the work of the highest vouched block (or the anchor;
  [why](../../docs/architecture.md#finality)). Peers never move that difficulty reference.
  `finalizable()` names the best-chain block at `min(highest settled, best − depth)`;
  `finalize(block)` moves every best-chain header up to it into the finalized headers and prunes
  every branch not descending from it. Finalizing anything else is a caller bug: it panics
  naming the invariant (H2, H6).
- Memory only: the newest max(`2 · depth`, `CONTEXT`) finalized headers stay, answering by height;
  older finalized headers are finalized by definition (`on_best` = true) and a header whose parent is one of
  them = `Orphan`.

## `HeaderChainSnapshot`

`HeaderChain::snapshot()` is an immutable value (clone = refcounts: `imbl` throughout). A
holder's answers never change (H5).

| Method | Answer |
| --- | --- |
| `best()` | the most-work tip |
| `finalized_tip()` | the last finalized block (always set: the anchor at least) |
| `hash_at(h)` / `header_at(h)` | the best chain above the finalized tip and the finalized headers kept; `None` above best or below the finalized headers kept |
| `on_best(at)` | `at` on the best chain: a held header's hash, or at/below the finalized tip past the finalized headers kept |
| `holds(at)` | `on_best(at)`, or a side block held above the finalized tip (the NFS prunes by it) |
| `vouched(h)` | a trusted validator witnessed the best-chain block at `h`; at or below the finalized tip = `true` (settled) |
| `locator()` | zcashd's `GetLocator`, ending at the finalized tip |

## Invariants, checked

| ID | Invariant |
| --- | --- |
| H1 | best = the max-work leaf, the first received on a tie |
| H2 | the tree descends from the finalized tip: nothing held before an anchor, each node one height above its parent, finalized headers one height apart and at most max(`2 · depth`, `CONTEXT`); only a best-branch block at least `depth` below best becomes finalized |
| H4 | at most `4 · depth` side-branch nodes and 32 side-branch tips |
| H5 | a published snapshot never changes; the best path is a held chain from the finalized tip |
| H6 | only a settled block (or an ancestor of one) becomes finalized |
| H7 | a parent holds every witness of its child, or is peer-quorate |
| H9 | a driver fetching nothing above `ceiling(batch)` keeps best − finalized ≤ `depth + batch` |

`HeaderChain::check()` asserts H1, H2, H4, H5, H7 and the tree's bookkeeping (cumulative work,
child counts, leaf set), naming the invariant in its panic; `finalize` asserts H2 and H6. H9 is
`zaino-chainview`'s header-sync test.

## Tests

- Captured headers (`examples/capture_headers.rs`): mainnet 0–300, 653,500–653,700 (Blossom),
  3,508,500–3,508,800; testnet 299,000–299,400 and 583,800–584,200. Every header passes
  `validate`, every nBits reproduced; each mutation of a real header refused by its own rule; a
  chain anchored mid-mainnet follows 200 trusted headers with no history below.
- Model (`random_header_trees_answer_like_the_naive_model`): random trusted trees with any nBits
  (orphans, side sprays past the bounds, trusted and per-netgroup peer witnesses, trusted runs,
  finalizations) against a naive tree; `check()` after every insert.
- Fire drills: each check in `check()` and each precondition, seen firing on a planted bug.
- Builder agreement (`testing`): random `MockChain` shapes, `MockChain::tip()` =
  `HeaderChain::best()` after each.
- For consumers' tests (`testing` feature), constructors over a `MockChain`:

  ```rust,ignore
  use zaino_header_chain::testing::{insert, insert_unwitnessed};
  use zaino_header_chain::{HeaderChain, HeaderChainSnapshot};

  let mut headers = HeaderChain::anchored(&chain, depth);          // anchored at genesis
  insert(&mut headers, &chain.blocks(tip))?;                       // a trusted run: its last vouched
  insert_unwitnessed(&mut headers, &peers_headers)?;               // nothing witnessed
  let snapshot = HeaderChainSnapshot::of(&chain, tip);             // finalized = genesis
  let pinned = HeaderChainSnapshot::finalized(&chain, tip, h(9));  // finalized through 9

  ```

Heavy run required after changes: see [docs/testing.md](../../docs/testing.md#heavy-run).
