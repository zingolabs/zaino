# zaino-service

Zaino's **inner driving surface**: the capability trait algebra an engine
implements and the serving adapters consume. One trait per capability, a pin
(`TakeSnapshot`) that makes reads coherent, and *use cases* layered on top that
name what each public consumer demands. Which provider answers each capability is not a
question this crate asks: that is routing, a choice the engine in `zaino-core`
makes per deployment.

## Three layers of availability

```text
required    ⊆   provided   ⊇   serviceable
(use case)      (the type)     (the manifest)
```

- **Required** is the use case's demand: a service such as `LightWalletService`
  is a bundle of read traits over a pin plus the controls that use case needs,
  blanket-implemented so a type *is* the service exactly when it has the parts.
- **Provided** is what the composed engine's type implements. Presence is
  compile-time: a read the providers cannot back under the chosen routing is an
  impl that does not exist, and the demand bound fails at the wiring site.
- **Serviceable** is `Serviceable::serviceability()` — per capability,
  `Absent | NotYet | ToHeight(h) | Live`. It can only narrow what the type
  permits, never widen it, and it derives from the same routing the reads use.

## Use cases are demand

`use_cases` defines one marker type per public consumer shape — `LightWallet`,
`NodeRpc`, `FullWallet` — with its demand beside it as a service trait
(`LightWalletService`, `NodeRpcService`, `FullWalletService`): a read-set over
a pin plus the controls that use case needs.

A read-set is not a use case. `read_sets` names bundles of read capabilities
(`LightWalletReads`, `NodeRpcReads`, `FullWalletReads`, over the shared
`WalletReadCore`), the pinned half of a demand, reusable across use cases.
Sibling read-sets share a core rather than inheriting from each other, so a
wallet-only addition never leaks into the served light protocol. `NodeRpcReads`
adds `ChainInfoRead`, whose `chain_info` answers the `getblockchaininfo`
aggregate as the validator's whole `BlockchainInfo` — one type describing one
chain position, not a reassembled subset. It also adds `BlockVerboseRead`
alongside `BlockRead`: `block_header_verbose` and `block_verbose` carry the
chain-position overlay (confirmations, difficulty, chainwork, neighbouring
hashes) a block's own bytes cannot give, kept a separate trait from `BlockRead`
for the same reason `RawTransactionRead` and `TransactionRead` are split — one
returns the domain block every consumer uses, the other an explorer-only
surface. The block page composes the two into one `getblock(_, 2)` response.
`BlockVerboseRead` also carries `raw_block`, the consensus bytes of the block
`at` — the always-passthrough `getblock(_, 0)` surface the explorer's search page
calls to test whether a string is a block. A selector naming no block is
`Ok(None)` at each of the three reads.

`NodeRpcReads` also carries `TransactionViewRead`, the resolved-transaction
surface behind `getrawtransaction <txid> 1` and `getblock <block> 2`:
`transaction_view` returns one transaction and `block_transaction_views` a whole
block, in both cases with every transparent input resolved to the
`TransparentOutput` it spends (the value and script the wire form shows). It is a
read distinct from `TransactionRead`, which returns the decoded transaction as it
stands: resolving a prevout needs a further lookup per distinct outpoint that a
wallet never asks for. A miss on the requested transaction or block is a domain
answer (`Ok(None)`), while a miss on a *prevout* is a source inconsistency —
`TransactionViewError::MissingPrevout`, naming the outpoint, never a blank value.
`TransactionViewError` is a typed enum, not a `read_error!` String type: its
`Unavailable` keeps the transport cause with `#[source]`, and
`PrevoutIndexOutOfRange` is its own variant for an out-of-range `prev_index`.
`PrevoutFanoutTooLarge` is a policy refusal, separate from `Unavailable`: while
prevouts are resolved by passthrough, a request whose distinct external prevouts
exceed the per-request ceiling is refused — naming the needed count and the
ceiling — before any fetch is issued, so one request cannot be amplified into an
unbounded number of validator round trips.

`NodeRpcReads` also carries `BlockHashRead`, the timestamp-range block selection
behind `getblockhashes` (`block_hashes(low, high)` returns every block with
`low <= nTime < high` as `BlockHashAt { height, hash, time }`, ascending by time
then by hash). It is a **local** read over the composed headers — same backing as
`HeaderRead`, no source port, and its availability is type-level rather than a
runtime capability (a tier implements `HeaderRead` only where its headers-index
backing exists). It is not a slice of the height axis: Zcash block
timestamps are not monotonic, so the engine derives a candidate height bracket
from the median-time-past consensus rule and filters it by each block's actual
time. A range beyond the tip or before genesis is an empty list, never an error;
a header missing from the chain view at or below the pinned tip is the typed
`BlockHashReadError::MissingHeader`, which keeps a tier-read cause under its
`TierRead` variant with `#[source]` rather than a flattened string.

`NodeRpcService` also demands `NodeStatusRead`, the node-operator status control
behind `getinfo`, `getmininginfo`, `getpeerinfo`, `getnetworksolps`,
`getdifficulty`, `getnetworkinfo` and `ping`. These are facts about the validator,
not the chain, so Zaino indexes none of them and all are passthrough — a single
`Capability::NodeStatus`, always `Live`. The reads are typed (`NodeInfo`,
`MiningInfo`, `Vec<PeerInfo>`, `u64`, `Difficulty`, `NetworkInfo`, and `()` for
`ping`), not an opaque string, so a serving adapter renders them into its own
wire shape. Their failure is the typed `NodeStatusError`: `NotReady` for a
validator still starting (the same request succeeds shortly) versus `Unreachable`,
which keeps the source-layer cause as a `#[source]` rather than flattening it to a
message.

`NodeRpcService` also demands `TxOutRead`, the `gettxout` control: a live,
passthrough lookup of an unspent transparent output. `Ok(None)` is the ordinary
answer for a spent or unknown outpoint (zcashd/zebra's `null`), never an error.
It is deliberately **not** `SpendRead` — that answers whether a known outpoint
was spent, a local index question, whereas this asks the validator "is this
unspent right now" against its current UTXO set.

`NodeRpcService` also demands `MempoolListing`, the one-shot mempool read behind
`getrawmempool` and `getmempoolinfo`. It is distinct from `MempoolSubscribe` (a
stream a wallet follows): node RPC asks what is in the mempool once and gets an
answer. Three methods answer three questions — `mempool_txids` (the cheap txid
listing), `mempool_entries` (per-transaction detail: `MempoolEntry` carries the
id, size, a typed `Zatoshis` fee, and entry height/time), and `mempool_summary`
(the count and total bytes behind `getmempoolinfo`). All are live passthrough to
the source that serves the mempool, never a finalised secondary; a validator with
no mempool is served as empty, and a transport failure is a `MempoolReadError`.

`Serves<U>` carries a use case's demand as a bound a generic wiring names
without naming the service trait; one blanket impl per use case forwards to
its service. A use case says nothing about how it is served: routing and the
index set are choices an implementation makes per deployment, below this
crate, and a second implementation may make them differently.

## Shared queries

`queries` is the domain-side composition more than one adapter would otherwise
each write — answered once, in one file, so two adapters cannot drift into two
answers for the same question.

Two **primitives** compute and nothing more: `serviceable_range(snapshot)` is
the height range a snapshot can answer (its `coverage`, not a tip-derived
guess), and `total_balance(snapshot, addrs, range)` sums transparent balances
over a range through the checked `Zatoshis` / `ZatoshisFlowSum` helpers,
reporting overflow rather than clamping it.

**Policy** — what an *unserviceable* snapshot means — is named per use case, not
left to the adapter, because consumers answer it differently: `address_balance`
(explorer) reads nothing-serviceable as a zero balance, while `wallet_balance`
returns `None` so a wallet never reports a user's funds as gone off an indexer
that cannot answer. An adapter calls only its own policy function, then renders
the domain answer to its wire shape and maps its error codes.

`address_deltas(snapshot, addrs, start, end)` answers the explorer's
address-history question beside `address_balance` and takes the same policy:
nothing serviceable means no indexed history, so an empty answer rather than an
error. Its `start` and `end` bounds are inclusive and optional, each defaulting
to the serviceable edge; backwards bounds answer empty for the same reason, since
callers derive them from user-supplied dates where a day with no blocks is
ordinary. The deltas are ordered `(height, block_index, index)` — the order
zcashd documents — applied once here rather than in each adapter. The returned
`AddressDeltasAnswer` carries that ordered list and the `range` actually queried,
which is `None` exactly when no query ran, so an adapter echoing the range has
the authoritative value rather than re-deriving it.

`address_txids(snapshot, addrs, start, end)` and `address_utxos(snapshot, addrs)`
compose the remaining insight-explorer address reads the same way, each ordered as
zcashd orders its equivalent. zcashd builds **one set across all the requested
addresses**, not one list per address: `getaddresstxids` sorts the union by
`(height, txid)` and de-duplicates (a transaction touching two of the addresses
appears once), and `getaddressutxos` sorts the merged unspent set by height. So
`AddressRead::tx_ids` carries each txid's height out of the read — a bare txid
cannot be re-sorted — and the query merges the per-address lists, sorts, and
returns the de-duplicated txids in that order; `address_utxos` merges and stably
sorts by height (`Utxo` already carries it). The carried height is
`Option<Height>`: a **local** read always knows it (`Some`), but a **passthrough**
source does not — zebra's `getaddresstxids` returns bare txids — so that path
reports `None` rather than a fabricated value. `address_txids` sorts the known
heights and keeps any unknown-height (`None`) entries in the validator's own
order, ahead of the known ones, inventing no height; the node-RPC deployment is
`Address = Local`, so in practice every entry carries `Some` and this is the plain
`(height, txid)` sort.
`address_txids` takes the inclusive optional bounds of `address_deltas`;
`address_utxos` is range-less — an unspent output is a fact about the current
chain, not a window of it — and both take the explorer policy: an unserviceable
snapshot answers empty, never an error.

## Errors

Every read error separates a *not-yet-serviceable* answer
(`NotServiceable(Capability)`) from a domain miss (`Ok(None)`, never an error)
and from backend failure (`Transient` / `Fatal`). The conformance kit
(`conformance`, behind `testing`) asserts that a use case's full read-set never
answers `NotServiceable` on an engine claiming its service.
