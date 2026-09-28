# zaino-service

Zaino's **inner driving surface**: the capability trait algebra an engine
implements and the serving adapters consume. One trait per capability, a pin
(`TakeSnapshot`) that makes reads coherent, and *profiles* layered on top that
name what a use case demands. Which provider answers each capability is not a
question this crate asks: that is routing, a choice the engine in `zaino-core`
makes per deployment.

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

## Errors

Every read error separates a *not-yet-serviceable* answer
(`NotServiceable(Capability)`) from a domain miss (`Ok(None)`, never an error)
and from backend failure (`Transient` / `Fatal`). The conformance kit
(`conformance`, behind `testing`) asserts that a profile's full read-set never
answers `NotServiceable` on an engine claiming that profile.
