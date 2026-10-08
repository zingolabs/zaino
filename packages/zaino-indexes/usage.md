# zaino-indexes

Zcash index definitions, the index sets that compose them, and the two things
a finalised store needs to know about what it holds: which indexes a deployment
builds (an **index set**), and which indexes each serving capability
composes from (a **local capability**).

## Index sets are types

`index_set::IndexSet` names an index set as a type, and
`Builds<I>` is type-level membership. The `index_set!` macro declares
one from a single index list and emits both the runtime `IndexPipelines` the
sync engine builds and the `Builds` impls the type promises, so the two
cannot drift:

```rust,ignore
index_set! {
    pub struct LightWallet over CurrentZainoContext {
        HeadersIndex, TxidsIndex, HashToHeightIndex, TransparentDataIndex,
        SaplingIndex, OrchardIndex, IronwoodIndex, ChainMetadataIndex,
    }
}
```

`sets::compact_blocks::CompactBlocks` is the compact-block set a lightwalletd
deployment needs; `sets::current_zaino::CurrentZaino` is the full set.
`sets::transparent_history::TransparentHistory` adds the indexes a local address
read composes from, and `sets::light_wallet_local::LightWalletLocal` adds the
`tree_state` index on top of those, so a light-wallet deployment serves both
address history and `GetTreeState` locally. A store reader is parametrised by one
of these (`StoreReader<B, M>`), and its serving reads exist only where `M` builds
what they compose from.

## Local capabilities are declared once

`capabilities::local` declares each capability the finalised store can back,
once, with the indexes it composes from:

```rust,ignore
local_capability! {
    Blocks = Blocks backed by [HeadersIndex, TxidsIndex, /* … */ ChainMetadataIndex]
}
```

That one declaration yields the bound a store read puts on its index set
(`M: Backs<local::Blocks>`) and the list the serviceability manifest checks
version stamps for (`local::Blocks::INDEXES`). The grain is per capability,
not per index: a capability composes several indexes and one index serves
several capabilities.

```text
has(M, C)        ⟺  indexes(C) ⊆ built(M)          rustc
advertises(M, C) ⟺  indexes(C) ⊆ stamped(backend)  snapshot time
```

`capabilities::serviceability(reader, watermark)` derives the store's manifest
from those lists: `ToHeight(w)` when every backing index is stamped and a
watermark is committed, `NotYet` when stamped but no watermark yet, `Absent`
otherwise — including for capabilities with no local index at all, which the
composer holding a passthrough provider widens.

`local::Treestate` (`tree_state` + `headers`) and `local::SubtreeRoots` (the
three `subtrees_*` indexes) are the treestate-serving capabilities, so a set
that builds them backs `TreestateRead` locally.

## Treestate domain primitives

`indexes::tree_state` holds the commitment-tree (treestate) domain layer and the
`tree_state` index built on it.

- `segment::TreeSegment<H>` is the ordered-monoid algebra over a contiguous run
  of note-commitment leaves, generic over a pool's Merkle hash `H`. `lift`
  builds a run's complete nodes (hashing each once, parallel across a level),
  `combine(a, b)` joins two adjacent runs with **at most one hash per level**
  along the seam (associative, not commutative), and `frontier_at(size)` reads
  any height's frontier as a pure lookup. `carry_segment` renders a stored
  carry frontier as the segment a batch combines onto.
- `pools::{sapling_leaf, orchard_leaf, ironwood_leaf}` convert a note
  commitment's bytes (`cmu` / `cmx`) to the tree's leaf hash, returning `None`
  for a non-canonical field encoding. Ironwood shares Orchard's Pallas leaf.
- `codec::legacy_tree_bytes` / `legacy_tree_from_bytes` are zcashd's legacy
  `CommitmentTree` encoding — the exact bytes `z_gettreestate`'s `finalState`
  carries — and its inverse. `codec::TreeStateValue` is a height's per-pool
  frontiers; `TreeStateIndex` is its `EntryCodec` (height key, `WalkOrdered`),
  persisting each pool as a big-endian size plus the v1 non-empty-frontier
  bytes. An all-empty value (every pool size 0) is a height no pool is active at;
  below a pool's activation height and active-but-empty are the same empty
  frontier here, distinguished at serve time from the network's activation
  heights.
- `sync::TreeStateIndex` is the wired `SelfCumulative<OrderedMonoid>` × Append
  index: height → `TreeStateValue`, built during a sync through the existing
  append-cumulative bridge's ordered-monoid scan over the three pools.
  `sync::TreeStateCtx` is its per-block input — each pool's note commitments in
  chain order — projected from `CurrentZainoContext`.
- `serve::pool_treestate` (and `TreeStateValue::pool_treestates`) render a
  stored frontier as the domain `PoolTreestate` — the root, the legacy
  `finalState` bytes, and the note-presence activation proxy (an empty tree is
  reported absent). Both the finalised store and the non-finalised window call
  it, so a treestate served either side of the seam renders identically.

## Subtree-roots indexes

`indexes::subtrees` holds the per-pool subtree-roots indexes backing
`GetSubtreeRoots` / `z_getsubtreesbyindex`: a completed subtree is the perfect
subtree of `2^16` consecutive note-commitment leaves, keyed by subtree index.

- `SubtreesIndex<P>` is one generic index parametrised by a `pool::Pool`
  (`SaplingPool` / `OrchardPool` / `IronwoodPool`, aliased
  `SaplingSubtreesIndex` etc.), so each pool is its own `WalkOrdered` namespace
  (`subtrees_sapling`, `subtrees_orchard`, `subtrees_ironwood`) with no
  duplicated logic. The key is the subtree index (`u32` big-endian);
  `codec::SubtreeRoot` is the value — a 32-byte root in internal (unreversed)
  order, the orientation every pool stores and serves, plus the completing
  height (`z_getsubtreesbyindex`'s `end_height`).
- It is a `CrossIndex` × Append over two declared dependencies — `tree_state`
  and the pool's compact index — because a subtree root domain-depends on both
  the commitment tree just before the completing block and that block's own
  leaves. Extraction reads `tree_state`'s frontier at `h−1` through the
  `DepsReader`, re-lifts this block's leaves onto it, and reads the frontier at
  each completion size, folding it to the level-16 node. Only the ~1,900 mainnet
  blocks that cross a `2^16` boundary emit anything; every other block emits an
  empty delta.
- The subtree level is a const 16 (`pool::SUBTREE_LEVEL`); `Pool::SUBTREE_LEVEL`
  defaults to it and is lowered only by a test pool, so completions can be driven
  with a handful of leaves.
