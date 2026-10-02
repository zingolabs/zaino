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
chain position, not a reassembled subset.

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

## Errors

Every read error separates a *not-yet-serviceable* answer
(`NotServiceable(Capability)`) from a domain miss (`Ok(None)`, never an error)
and from backend failure (`Transient` / `Fatal`). The conformance kit
(`conformance`, behind `testing`) asserts that a use case's full read-set never
answers `NotServiceable` on an engine claiming its service.
