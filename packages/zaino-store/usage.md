# zaino-store

The finalised tier as a provider: a thin reader over a persistence backend and
an index set that composes the `zaino-service` reads on read and pins every
read to the writer's committed watermark.

It is an adapter, not a ports crate. The ports it serves are the shared
serving ports the chain head also serves (`ChainSegment`, `CompactBlockRead`,
`HeaderRead`, `Serviceable`, `TakeSnapshot`), so the composer in `zaino-core`
sees both tiers through one contract; the ports it consumes are
`zaino-persistence`'s `Backend` and the index schemas in `zaino-indexes`.

## Presence is in the type

`StoreReader<B, M>` is typed to the index set `M` it reads, and a serving read
exists on its snapshot only where `M` builds every index that read composes
from (`M: Backs<local::Blocks>` for both the compact block and its
`HeaderRead` header projection, which the headers index — one of the `Blocks`
indexes — backs). A store over a set that
lacks an index has no such read, so a deployment that needs it fails at the
wiring, not at a request. Reads the store does not own are not stubbed; the
composer routes them to the provider that has them.

## Watermark repair

The watermark is a stamp beside the data and the data outranks it. On boot
the runtime calls `StoreReader::repair_watermark`, which finds the highest
header held, scans a window below it for holes, and re-stamps at the top of
the unbroken run in one atomic write. A stamp above the data would otherwise
route a gap of heights to the store and resume the indexer past them.

## Runtime component

The supervised-component wrapper that presents this reader to the Orchestra
lives in the `zaino-store-service` crate.
