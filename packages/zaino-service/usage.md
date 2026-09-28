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
wallet-only addition never leaks into the served light protocol.

`Serves<U>` carries a use case's demand as a bound a generic wiring names
without naming the service trait; one blanket impl per use case forwards to
its service. A use case says nothing about how it is served: routing and the
index set are choices an implementation makes per deployment, below this
crate, and a second implementation may make them differently.

## Errors

Every read error separates a *not-yet-serviceable* answer
(`NotServiceable(Capability)`) from a domain miss (`Ok(None)`, never an error)
and from backend failure (`Transient` / `Fatal`). The conformance kit
(`conformance`, behind `testing`) asserts that a use case's full read-set never
answers `NotServiceable` on an engine claiming its service.
