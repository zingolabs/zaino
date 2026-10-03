# zaino-core

The engine every use case is served by: the finalised store, the non-finalised
chain head and the validator, composed into one `zaino-service` `IndexerService`
**under a routing**. One engine type backs every use case; what differs per
deployment is the routing it is composed under and the store it is composed
over, both type parameters.

```text
Engine<Fs, Nfs, Src, R>
   Fs   finalised store     TakeSnapshot, snapshot: ChainTier (+ index reads)
   Nfs  non-finalised head  TakeSnapshot, snapshot: ChainTier (+ window reads)
   Src  validator handle    the canonical, resilient zaino-source ports
   R    routing             zaino_core::routing::Routing — who answers what
```

## Routing is placement, per deployment

`routing::Routing` is a type with one `Placement` — `Local`, `Passthrough` or
`Withheld` — per capability whose placement is a decision. Compact blocks are
always local and raw transactions, broadcast, mempool and the upgrade schedule
are always the validator's, so they are not on it. `placement()` is exhaustive
over `Capability`: a new variant must be classified.

```rust,ignore
pub struct LightWalletRouting;
impl Routing for LightWalletRouting {
    type Address = Passthrough;       // relayed to the validator, for now
    type Treestate = Passthrough;
    type Spend = Withheld;            // a node read; not offered
    type TransactionLocation = Withheld;
}
```

`NodeRpcRouting` is the node-RPC / explorer deployment's routing. The node reads
the explorer adds (full and verbose blocks, decoded transactions, the chain-info
aggregate, the node-status reads) are always passthrough and so are not on the
table at all; what the table decides is that address history is served locally:

```rust,ignore
pub struct NodeRpcRouting;
impl Routing for NodeRpcRouting {
    type Address = Local;             // served from Zaino's own transparent indexes
    type Treestate = Passthrough;
    type Spend = Withheld;            // re-added with a local spend index later
    type TransactionLocation = Withheld;
}
```

Address history is `Local` because the explorer's address page needs
`getaddressdeltas` — full transparent history, receives and spends — which no
validator answers in plain RPC mode: Zebra has no such method. The deployment
therefore indexes transparent history itself (the `TransparentHistory` set) and
discloses no queried addresses to the validator. `Spend` stays `Withheld`: the
window's spend data is consumed internally through `AddressReceiveRead`, not the
engine `Spend` placement, so no served method needs a `Local` spend read.

Routing is a property of the deployment, not of the use case it serves: the
demand traits in `zaino-service` say nothing about placement, so a second
engine could serve the same use case under a different table. Flipping a
placement is a one-line change to the routing type. Whatever that placement
needs and the providers lack is then a compile error at the wiring, naming the
missing port.

## What the composer decides, and what it does not

The composer decides *how* two chain tiers become one coherent view: both are
captured together on each pin (the `chain_view` module, see
[chain_view.md](./chain_view.md)), so the seam watermark and the volatile
window agree, and every read through the resulting `EngineSnapshot` sees one
instant.

It does **not** decide which provider answers a capability. That is `R`. Each
read trait is implemented on the snapshot once, bounded on `R`'s placement for
that capability and on the provider ports that placement needs:

| capability | placement | needs |
|---|---|---|
| compact blocks, nullifier projection | always local | the two tiers' compact reads |
| header projection (hash + time by height) | always local | the two tiers' `HeaderRead` |
| timestamp-range block hashes (`getblockhashes`) | always local | the two tiers' `HeaderRead` |
| full blocks (the tip excepted) | always passthrough | `GetBlock`, `GetBlockByHash` |
| verbose header / block (chain position) | always passthrough | `GetBlockHeader`, `GetBlockVerbose`, `GetBlockVerboseByHash` |
| chain-info aggregate | always passthrough | `GetBlockchainInfo` |
| pool-decomposed transaction and its status | always passthrough | `GetTransactionVerbose` |
| raw transaction, broadcast, mempool, upgrades | always passthrough | the source ports |
| address history | `R::Address` | `Local`: finalised store `AddressRead`, head `AddressReceiveRead` (receives + the spends it saw), threaded across the seam; `Passthrough`: the four address source ports |
| treestate, subtree roots | `R::Treestate` | `Passthrough` only today; a local tree index adds a `Local` impl beside it |
| spend status | `R::Spend` | `Local` only: both tiers `SpendRead` |

`BlockRead` (full `Block`, header, by-hash height, and the ascending
`stream_blocks` over an inclusive range) is always passthrough: the finalised
store keeps compact projections, not full block bytes. Its one exception is
`tip`, read locally off the pinned view — it is the coordinate the rest of the
snapshot is coherent against, and the validator's tip is one the snapshot does
not share. A domain miss (an unknown height or hash) is `Ok(None)`; an
unreachable validator is a transient read failure, never a miss.

`TransactionRead` (the pool-decomposed transaction and its `transaction_status`)
is always passthrough: the store holds no transaction bytes, and the pool
decomposition needs the validator's chain library, which this crate must not
depend on, so the decoding lives in the source adapter. `transaction_status`
maps `BestChain(h)` to `Mined(h)`, `NonBestChain` to `Orphaned`, and both
`Mempool` and an absent transaction to `Unknown` — a mempool transaction is
neither mined nor reorged out.

`BlockVerboseRead` (`block_header_verbose`, `block_verbose` over a selector, and
`raw_block` over a selector) is always passthrough: confirmations, difficulty,
chainwork and the neighbouring hashes are cumulative chain state the validator
derives, not facts in the stored block, and the raw consensus bytes `raw_block`
returns — the `getblock(_, 0)` surface — come over `GetRawBlock`/`GetRawBlockByHash`
because the finalised store holds compact projections, not full block bytes. A
domain miss is `Ok(None)`; an unreachable validator is a transient read failure.

`BlockHashRead` (`block_hashes(low, high)`, the `getblockhashes` timestamp-range
selection) is always local, driven over the two tiers' `HeaderRead` and clamped
to the pinned tip. Its availability is type-level rather than a runtime capability:
each tier implements `HeaderRead` only where its headers-index backing exists.
Zcash block timestamps are not monotonic, so this is not a
slice of the height axis: the engine drives the pure `CandidateSearch` from
`zaino-consensus` — which derives, from the median-time-past consensus rule, the
smallest height bracket guaranteed to contain every block with
`low <= nTime < high` — then scans that bracket and keeps the blocks whose own
timestamp is in range, ascending by time then by hash. The drift rule is the
mainnet one (`MaxBlockTimeDrift::MAINNET`), the network the explorer deployment
serves; the engine carries no network, so a testnet deployment would need the
testnet drift here. A range beyond the tip is an empty list, never an error; a
header missing from the view at or below the tip is a typed
`BlockHashReadError::MissingHeader`.

With `BlockRead`, `BlockVerboseRead`, `BlockHashRead` and `TransactionRead` all
present, `EngineSnapshot` satisfies `NodeRpcReads`, so a node-RPC-routed engine
satisfies `NodeRpcService`.

`ChainInfoRead` (the `getblockchaininfo` aggregate) is always passthrough, as one
piece: the aggregate describes a single chain position, so mixing a locally-read
height with a passed-through value pool would report two different heights in one
answer. It moves local in one piece once a value-pool cumulative bridge exists.
An unreachable or still-starting validator is a transient read failure, never a
defaulted success — a blanked aggregate would wipe a polling consumer's views
while the previous answer was still valid.

A placement whose providers are missing is an impl that does not exist, so the
use case's demand bound fails where the engine is wired. The crate docs on
`Engine` carry four doc-tests that pin this: the light routing over a
light-wallet store serves the light-wallet use case; the same store with address history
routed locally is not (compile-fail, the store lacks the index); local address
history over providers that have it is; and treestate routed locally is not
for any providers.

## The manifest follows the routing

`Serviceable::serviceability()` derives from `R` and the finalised store's own
manifest: withheld → `Absent`, passthrough → `Live`, local → whatever the store
reports for that capability. Reach through the non-finalised window is not
folded in yet (serviceability is an engine port; the head's tip is a property
of the pin), and a passthrough capability reports `Live` unconditionally until the
validator's reachability probe is threaded through.

## Merging across the seam

A `Local` placement answers a range by splitting it at the watermark: heights
`≤ w` from the finalised store, above it from the head, joined. For address
history that is a concatenation of deltas and txids, a checked sum of balances
(`Zatoshis::checked_add`, `ZatoshisFlowSum::checked_join`), and store UTXOs
filtered through the head's spend status. The split is `split_at_seam` in the
snapshot module, tested on its own.
