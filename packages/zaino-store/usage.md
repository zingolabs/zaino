# zaino-store

The finalised tier as a provider: a thin reader over a persistence backend and
an index set that composes the `zaino-service` reads on read and pins every
read to the writer's committed watermark.

It is an adapter, not a ports crate. The ports it serves are the shared
serving ports the chain head also serves (`ChainSegment`, `CompactBlockRead`,
`Serviceable`, `TakeSnapshot`), so the composer in `zaino-core` sees both
tiers through one contract; the ports it consumes are `zaino-persistence`'s
`Backend` and the index schemas in `zaino-indexes`.

## Presence is in the type

`StoreReader<B, M>` is typed to the index set `M` it reads, and a serving read
exists on its snapshot only where `M` builds every index that read composes
from (`M: Backs<local::Blocks>` for compact blocks). A store over a set that
lacks an index has no such read, so a deployment that needs it fails at the
wiring, not at a request. Reads the store does not own are not stubbed; the
composer routes them to the provider that has them.

## Spend status

`SpendRead` exists on a store whose set builds `local::SpendStatus`'s three
indexes, and reports one of three answers for an outpoint:

| answer | means |
| --- | --- |
| `Spent { by }` | the spends index holds a spend of it |
| `Unspent` | no spend recorded, and the output exists at or below the watermark |
| `NoSuchOutput` | no spend recorded, and nothing in the finalised range created it |

The third index set is why the capability names three indexes. Absence from
the spends index is ambiguous on its own — an outpoint the finalised range
never created is absent exactly as an unspent one is — so absence is resolved
against the output's existence, through the txid's location and that block's
transparent data.

`Unspent` means unspent **as of the watermark**. The volatile window above it
may have spent the output since, which is the composer's business: `zaino-core`
asks the head first and falls through to the store on anything but a spend.

## Watermark repair

The watermark is a stamp beside the data and the data outranks it. On boot
the runtime calls `StoreReader::repair_watermark`, which finds the highest
header held, scans a window below it for holes, and re-stamps at the top of
the unbroken run in one atomic write. A stamp above the data would otherwise
route a gap of heights to the store and resume the indexer past them.

## Runtime component

`StoreComponent` wraps the reader as a supervised component: it is `Ready`
once opened and reports the watermark as progress.
