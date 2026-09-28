# zainod

The daemon: the composition root that turns a `DaemonConfig` into a running
Zaino. It is the only crate that names concrete adapters, and the only place a
runtime value becomes a type.

## Deployments

A daemon runs one **deployment**, selected by config:

```toml
use_case = "light-wallet"   # the default; the only variant today
```

A use case (`zaino_service::use_cases`) names what a consumer demands. A
deployment is a type in `zainod::use_case` naming how this daemon meets it,
three things the compiler checks together:

- the **use case** it serves (`LightWallet`), carried as the `Serves<U>`
  bound on its service trait (`LightWalletService`);
- the **routing** — which provider answers each capability
  (`zaino_core::routing::LightRouting`);
- the **index set** — which indexes the finalised store builds
  (`zaino_indexes::sets::light_wallet::LightWallet`).

`indexer::select_use_case` is the one `match` over `UseCaseKind`; each arm
names a deployment and the serving adapter that speaks its use case's
protocol, and nothing else. `boot` is generic over the deployment: the
namespaces the backend opens, the set the indexer builds, the type the store
reader is wired to and the routing the engine composes under all come from
`D`, so they cannot be paired wrongly. `use_case::compose` carries the single
`demand ⊆ supply` check as its `where` clause; an index set lacking an index
the demand needs, or a placement no provider can take, fails there.

Config **selects** a deployment. It does not shape one: what is built on disk
and which provider answers a query are properties of the type, not knobs. See
the design note on the three roles of config for what a runtime placement knob
would cost.

## Validators

The validator is the second supply axis beside the index set. The daemon
builds one `ValidatorClient` over one shared validator adapter and hands that
same client to every consumer — the indexer, the chain head, the engine's
passthrough — so nothing above the client touches a single-attempt port and
retrying happens in exactly one place.

What a validator must provide is named twice, as bundles on the client:

- `use_case::DaemonSource` — the floor to boot at all: the chain head's source
  port and the compact-block indexer's, both over the canonical ports.
- `use_case::LightWalletSource` — the floor plus every port the light routing
  relays to the validator. One such bundle per deployment, beside it.

`[source]` in config selects the adapter and its transport; today both arms
build a Zebra adapter. A new adapter — a zcashd-contract JSON-RPC one, say —
implements the `OneShot*` ports it can, and the compiler says which
deployments the client over it can serve: an arm whose bundle names a port the
adapter lacks does not compile.

## Adding a deployment

1. A use case in `zaino-service` (a marker, a read-set, a service trait and its
   `Serves` impl), if none fits.
2. A routing type in `zaino-core`, one placement per varying capability, if
   no stock table fits.
3. An index set in `zaino-indexes`, one index list.
4. In `use_case.rs`: the marker type and its `Deployment` impl naming the
   three.
5. A `UseCaseKind` variant and one arm in `select_use_case` naming the adapter.

Existing deployments do not change.

## Boot-time checks

Two things the daemon does before the indexer resumes, both loud when they
fire:

- **Watermark repair.** The store's watermark is a stamp beside the data. If
  it claims a height the headers index does not hold, every height in the gap
  would be routed to the store and answered "no such block", and the indexer
  would resume past heights it never indexed. The daemon re-stamps it at the
  highest header held and logs a warning naming both heights.
- **Tip polling.** Neither Zebra transport pushes tip changes, so the daemon
  hands the validator a second RPC handle to poll every two seconds. Without
  it the finalised indexer's follow loop has nothing to wake it and the index
  stays at its catch-up height; the driver warns if it is ever handed a source
  it cannot follow.

