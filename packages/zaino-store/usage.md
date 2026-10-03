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

## Transparent address history

`AddressRead` exists on a store whose set builds `local::AddressHistory`'s four
indexes, and answers the whole read: gross receipts, the unspent set, balance
changes and the transactions that caused them.

Four indexes, because the address index is the receive side alone — address →
the outputs that paid it — and everything else is composed on read. Netting a
balance needs the spends index. Reporting a spend as a *delta* needs the
location a balance change is attributed to, which is the spending
transaction's height and position from the txid-location index and which of
its inputs consumed the outpoint from that block's transparent data.

An unspent output's script is not fetched. A standard script is determined by
the address it pays, and only a standard script could have matched the queried
address in the index, so the script is reconstructed from the address.

The finalised tier is the only one that implements this read. The volatile
window cannot: a transparent input names the outpoint it consumes, not the
address that outpoint paid, so for an output created below its floor the window
has nothing to attribute the spend to. It implements the narrower
`AddressReceiveRead` instead, and `zaino-core` composes the two.

## Watermark repair

The watermark is a stamp beside the data and the data outranks it. On boot
the runtime calls `StoreReader::repair_watermark`, which finds the highest
header held, scans a window below it for holes, and re-stamps at the top of
the unbroken run in one atomic write. A stamp above the data would otherwise
route a gap of heights to the store and resume the indexer past them.

## Runtime component

The supervised-component wrapper that presents this reader to the Orchestra
lives in the `zaino-store-service` crate.
