# zaino-service

Zaino's **inner driving surface**: the capability trait algebra an engine
implements and the serving adapters consume. One trait per capability, a pin
(`TakeSnapshot`) that makes reads coherent, and two things layered on top —
*profiles* that name what a use case demands, and *routing* that names which
provider answers each capability for that use case.

## Three layers of availability

```text
required    ⊆   provided   ⊇   serviceable
(profile)       (the type)     (the manifest)
```

- **Required** is the use case's demand: a profile such as `LightServeService`
  is a bundle of read traits over a pin plus the controls that use case needs,
  blanket-implemented so a type *is* the profile exactly when it has the parts.
- **Provided** is what the composed engine's type implements. Presence is
  compile-time: a read the providers cannot back under the chosen routing is an
  impl that does not exist, and the profile bound fails at the wiring site.
- **Serviceable** is `Serviceable::serviceability()` — per capability,
  `Absent | NotYet | ToHeight(h) | Live`. It can only narrow what the type
  permits, never widen it, and it derives from the same routing the reads use.

## Profiles are demand

`profiles` names two altitudes. Read-sets (`WalletReadCore`,
`LightWalletReads`, `NodeRpcReads`) are the reads a use case pulls through a
pinned view. Service profiles (`WalletLibService`, `LightServeService`,
`NodeRpcService`) compose a read-set with the controls. Sibling profiles share
a core rather than inheriting from each other, so a wallet-only addition never
leaks into the served light protocol.

## Routing is placement, per use case

`routing::Routing` is a type with one `Placement` — `Local`, `Remote` or
`Withheld` — per capability whose placement is a decision. Compact blocks are
always local and raw transactions, broadcast, mempool and the upgrade schedule
are always remote, so they are not on it. `placement()` is exhaustive over
`Capability`: a new variant must be classified.

```rust,ignore
pub struct LightRouting;
impl Routing for LightRouting {
    type Address = Remote;            // passed through, for now
    type Treestate = Remote;
    type Spend = Withheld;            // a node read; not offered
    type TransactionLocation = Withheld;
}
```

A composer (see `zaino-store-service`) implements each routed read trait once,
dispatching to a per-capability *placement trait* implemented on the markers
themselves: `Local` carries the bounds a merge across the seam needs of the two
chain tiers, `Remote` the source ports a passthrough needs, `Withheld` nothing.
Handlers bound on the read trait and never learn which one they got.

Flipping a placement is a one-line change to the routing type. Whatever that
placement needs and the providers lack is then a compile error at the wiring,
naming the missing port.

## Errors

Every read error separates a *not-yet-serviceable* answer
(`NotServiceable(Capability)`) from a domain miss (`Ok(None)`, never an error)
and from backend failure (`Transient` / `Fatal`). The conformance kit
(`conformance`, behind `testing`) asserts that a profile's full read-set never
answers `NotServiceable` on an engine claiming that profile.
