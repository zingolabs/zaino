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
