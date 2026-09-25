# zaino-indexes

Zcash index definitions, the index sets that compose them, and the two things
a finalised store needs to know about what it holds: which indexes a deployment
builds (a **materialisation**), and which indexes each serving capability
composes from (a **local capability**).

## Materialisations are types

`materialisation::Materialisation` names an index set as a type, and
`Builds<I>` is type-level membership. The `materialisation!` macro declares
one from a single index list and emits both the runtime `IndexSet` the sync
engine builds and the `Builds` impls the type promises, so the two cannot
drift:

```rust,ignore
materialisation! {
    pub struct LightWallet over CurrentZainoContext {
        HeadersIndex, TxidsIndex, HashToHeightIndex, TransparentDataIndex,
        SaplingIndex, OrchardIndex, IronwoodIndex, ChainMetadataIndex,
    }
}
```

`sets::light_wallet::LightWallet` is the compact-block set a lightwalletd
deployment needs; `sets::current_zaino::CurrentZaino` is the full set. A store
reader is parametrised by one of these (`StoreReader<B, M>`), and its serving
reads exist only where `M` builds what they compose from.

## Local capabilities are declared once

`capabilities::local` declares each capability the finalised store can back,
once, with the indexes it composes from:

```rust,ignore
local_capability! {
    Blocks = Blocks backed by [HeadersIndex, TxidsIndex, /* … */ ChainMetadataIndex]
}
```

That one declaration yields the bound a store read puts on its materialisation
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
