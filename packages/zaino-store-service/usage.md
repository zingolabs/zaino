# zaino-store-service

The concrete inner engine: the finalised store, the non-finalised chain head
and the validator, composed into one `zaino-service` `IndexerService` **under a
use case's routing**.

```text
Composed<Fs, Nfs, Src, R>
   Fs   finalised store     TakeSnapshot, snapshot: ChainSegment + CompactBlockRead (+ index reads)
   Nfs  non-finalised head  TakeSnapshot, snapshot: ChainSegment + CompactBlockRead (+ window reads)
   Src  validator handle    the canonical, resilient zaino-source ports
   R    routing             zaino_service::routing::Routing — who answers what
```

## What the composer decides, and what it does not

The composer decides *how* two chain tiers become one coherent view: both are
captured together on each pin (`zaino-chainview`), so the seam watermark and
the volatile window agree, and every read through the resulting
`ComposedSnapshot` sees one instant.

It does **not** decide which provider answers a capability. That is `R`. Each
read trait is implemented on the snapshot once, bounded on `R`'s placement for
that capability and on the provider ports that placement needs:

| capability | placement | needs |
|---|---|---|
| compact blocks, nullifier projection, chain info | always local | the two tiers' compact reads |
| raw transaction, broadcast, mempool, upgrades | always remote | the source ports |
| address history | `R::Address` | `Local`: both tiers `AddressRead`, head `SpendRead`; `Remote`: the four address source ports |
| treestate, subtree roots | `R::Treestate` | `Remote` only today; a local tree index adds a `Local` impl beside it |
| spend status | `R::Spend` | `Local` only: both tiers `SpendRead` |

A placement whose providers are missing is an impl that does not exist, so the
use case's profile bound fails where the engine is wired. The crate docs on
`Composed` carry four doc-tests that pin this: the light routing over a
light-wallet store is the light profile; the same store with address history
routed locally is not (compile-fail, the store lacks the index); local address
history over providers that have it is; and treestate routed locally is not
for any providers.

## The manifest follows the routing

`Serviceable::serviceability()` derives from `R` and the finalised store's own
manifest: withheld → `Absent`, remote → `Live`, local → whatever the store
reports for that capability. Reach through the non-finalised window is not
folded in yet (serviceability is an engine port; the head's tip is a property
of the pin), and a remote capability reports `Live` unconditionally until the
validator's reachability probe is threaded through.

## Merging across the seam

A `Local` placement answers a range by splitting it at the watermark: heights
`≤ w` from the finalised store, above it from the head, joined. For address
history that is a concatenation of deltas and txids, a checked sum of balances
(`Zatoshis::checked_add`, `ZatoshisFlowSum::checked_join`), and store UTXOs
filtered through the head's spend status. The split is `split_at_seam` in the
snapshot module, tested on its own.
