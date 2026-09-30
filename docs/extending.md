# Extending the runtime

How to carry out the common changes in the stack's present shape. Each recipe
names the files to touch, in order, and what the compiler checks for you.

## The four layers

| layer | crate | what lives there |
| --- | --- | --- |
| demand | `zaino-service` | use cases, the reads and controls each one needs, the capability axis |
| engine | `zaino-core` | the composition of store ⊕ head ⊕ validator, and the routing that decides which answers what |
| supply | `zaino-runtime` | deployments: one way of serving a use case, and the assembly that boots it |
| composition root | `zainod` | which deployments are on offer, config, and the adapters that speak each protocol |

The direction is one-way. Demand never knows how it is met; supply names the
demand it meets. A change belongs to the highest layer that can express it.

Two sentences carry most of the design:

- **A use case is demand.** It says what reads and controls a consumer needs,
  nothing about where they come from.
- **A deployment is one supply.** It picks a routing, an index set and a
  config, and claims to meet one use case. Two deployments may meet the same
  use case differently, and both are correct.

The single check that holds it together is the `where` clause on
`zaino_runtime::deployment::compose`. A deployment whose parts cannot answer
its use case's demand fails there, at one call site, naming what is missing.
It cannot fail at a request.

With one caveat worth knowing before you rely on it: that check fires where the
assembly is **instantiated with a concrete validator**. A deployment no binary
boots is never instantiated, so nothing checks it. Give each deployment a test
that resolves `Serves<UseCase>` for its engine over a real validator type, and
the check is back.

## Add a new use case

For a consumer shape that does not exist yet: a block explorer feed, a mining
pool endpoint, a wallet library embedding.

1. **`zaino-service/src/use_cases/<name>.rs`** — a marker struct, `impl UseCase`
   with a `NAME`, a `<Name>Service` trait bundling the read set with the
   controls it needs, a blanket impl of that trait, and
   `impl<S: <Name>Service> Serves<<Name>> for S`. Export it from
   `use_cases.rs`.
2. **`zaino-service/src/read_sets.rs`** — if its reads differ from an existing
   set, add one. Read sets compose existing read traits; they are not use
   cases, and several use cases may share one.
3. **`zaino-core/src/routing.rs`** — a `Routing` impl, if its placements differ
   from an existing one. Do not edit an existing routing to suit a new use
   case.
4. **`zaino-runtime/src/deployment/<name>.rs`** — a deployment struct with
   `impl Deployment` naming the use case, routing and index set, `impl
   RuntimePlan` naming its config type and readiness criteria, and a
   `<Name>Source` trait bundling everything it needs of the validator.
5. **A serving adapter** speaking its protocol, the way `zaino-lightserve`
   speaks the light-wallet gRPC — **only if the use case has a wire
   protocol**. A use case consumed in process has no server to boot. Its
   adapter is the library its consumer calls, as `zaino-wallet` is for
   `FullWallet`, and its deployment boots through `boot_embedded`, which
   assembles everything and hands back the engine instead of supervising a
   server.
6. **`zainod`** — a `DeploymentKind` variant and a match arm in
   `select_deployment` that calls `boot_indexed` with the deployment and builds
   the adapter. An embedded use case skips this step too: nothing in the daemon
   selects it, because the embedding consumer is the one that boots it.

The source bundle in step 4 is hand-kept, because Rust cannot compute the union
of source bounds the routing's impls select. It is safe to get wrong in one
direction only: a missing port fails the demand bound at the wiring and names
itself.

## Make an existing use case configurable

First decide which kind of knob it is.

**A knob that tunes** — batch size, concurrency, a store path, a fetch
strategy — is config. Add a field to the deployment's config type
(`zaino-runtime/src/config.rs`, today `IndexedDeploymentConfig` composing
`StoreConfig` and `IndexerConfig`), read it in `boot_indexed`, and surface it
in zainod's file and env. Adapter-owned settings belong to the adapter's own
section, not to the daemon.

**A knob that decides who answers** is not config. It is a second deployment.
Routing is a type, so the demand check happens at compile time; a runtime
switch between two routings would move that check to a request. Write the
sibling deployment and let config select it by `DeploymentKind`.

The rule: **config tunes, deployment decides.**

## Change routing for one use case only

Routing is per deployment, so never edit a shared routing to change one
deployment's behaviour.

1. Add a `Routing` impl in `zaino-core/src/routing.rs` with the placements you
   want.
2. Add a deployment naming it, with whatever index set the new placements
   require.

Both deployments coexist and both remain selectable. The worked example is in
the tree: `LightWalletRouting` has `type Address = Passthrough`, which
discloses queried addresses to the validator. A sibling routing with
`type Address = Local`, plus an index set that builds the address history
index, is the private version. That is a new pair of types, not an edit.

## Add a capability to a use case

### When the capability already exists

"Exists" has three states, and they are not the same. A capability may have a
`Capability` variant, may have a read trait in `zaino-service/src/reads.rs`,
and may have an engine impl that can actually answer it. Check all three before
assuming the work is wiring. `TransactionLocation` has a variant and a trait
but no engine dispatch; `BlockRead` has a trait and neither of the others.

1. Add the read trait to that use case's read set, or to its `<Name>Service`
   trait if it is a control rather than a read.
2. Every deployment meeting that use case must now supply it. For each, either
   the index set gains the backing index and the routing places it `Local`, or
   the routing places it `Passthrough` and the deployment's source bundle gains
   the port.

The compiler points at `compose` for each deployment that cannot answer.

### When the capability is new

1. **`zaino-service/src/capability.rs`** — a `Capability` variant. The manifest
   iterates the enum, so it picks the variant up with no hand-kept list.
2. **`zaino-service/src/reads.rs`** — the read trait, returning domain types.
3. **`zaino-service/src/read_sets.rs`** — add it to the sets that demand it.
4. **`zaino-core/src/routing.rs`** — classify it in `Routing::placement`. That
   match is exhaustive over `Capability`, so the code does not compile until
   you have. Either fix the placement, as `Blocks` is always `Local` and
   `Broadcast` always `Passthrough`, or add an associated type if the placement
   is a per-use-case decision.
5. **The provider.** For `Local`: an index in `zaino-indexes` and a read impl
   in `zaino-store`. For `Passthrough`: a port in `zaino-source` and the
   relaying impl on the passthrough provider.

## Flip a capability between local and passthrough

One line in the routing impl, changing the associated type, **if both providers
already exist**.

- To `Local` you need the index in the deployment's index set, **and the read
  implemented on both tiers** — the finalised store's snapshot and the
  non-finalised head's. A `Local` placement bounds both, because a locally
  composed answer is stitched across the seam. An index without the two impls
  is data nothing can serve.
- To `Passthrough` you need the port in the deployment's source bundle.

**As the tree stands, `Local` is reachable for compact blocks only.** Both
tiers implement `ChainSegment` and `CompactBlockRead` and nothing else, so
every other capability can be `Passthrough` or `Withheld` today whatever its
index set builds. The address-history, spend and transaction-location indexes
exist and are declared as local capabilities, but no tier reads them out.
Making any of them serviceable is the two tier impls, not a routing edit.

The serviceability manifest follows automatically: a passthrough capability
reports `Live`, a local one reports `ToHeight` as far as its tiers reach. Reads
and manifest consult one declaration, so they cannot disagree.

To flip it for one deployment only, add a routing and a deployment rather than
editing the shared one, as above.

## Withhold a capability

Place it `Withheld`. The manifest reports `Absent`, which tells a client that
waiting will not help but configuration might, and reads return
`NotServiceable`. This is how a deployment declines to offer something its
providers could technically answer.

## What the compiler checks, and where it complains

| mistake | where it fails |
| --- | --- |
| an index set lacking an index the demand's reads need | `compose`, in the deployment's boot |
| a placement no provider can take | `compose` |
| a validator missing a port a passthrough placement relays | the deployment's source bundle, at the daemon's arm |
| a new capability nobody classified | `Routing::placement`, non-exhaustive match |
| a manifest silent about a capability | impossible: derivation iterates the enum |

Nothing on this list can reach a request. That is the property the layering
exists to buy, and the reason a change that looks like configuration is
sometimes a second deployment instead.

## What each change makes everyone else recompile

The question a reviewer asks next. The shared surfaces are all in the demand
and engine layers: the capability axis, the routing vocabulary, the placement
dispatch, and the read sets more than one use case composes. Supply is where
things multiply without colliding.

**Adding a new capability is the one change that must touch the shared core.**
It reaches the `Capability` enum, the `Routing` trait and its exhaustive
`placement` match, a new placement trait with its three impls, and a read impl
on the engine snapshot. Every use case's manifest gains an entry and every
routing must classify the variant. The reach is real but bounded and
compiler-visible: the match does not compile until the variant is classified,
and a missing placement impl names itself.

**Two changes touch shared code only if you edit in place instead of adding a
sibling.** Changing a routing, and flipping a capability between local and
passthrough, are additive when written as a new `Routing` impl; editing an
existing one changes every deployment that names it. Adding an existing
capability to a use case is local when it goes into that use case's own read
set, and shared when it goes into one that several use cases compose, such as
`WalletReadCore`.

**The rest are additive.** A new use case adds types in every layer and changes
nothing existing beyond module exports and the daemon's variant and arm. A
tuning knob is local to one deployment's config, with one caveat: a field added
to `IndexedDeploymentConfig` reaches every deployment that builds a local
index. A new field with a serde default stays compatible, but the type is
shared.

The shape to take from this: "add a deployment" is almost always cheaper than
"add a switch", because a deployment is supply and supply multiplies freely,
while a switch lands in demand or in the engine, where everyone pays for it.
