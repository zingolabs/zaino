# zainod

The daemon: the composition root that turns a `DaemonConfig` into a running
Zaino. It is the only crate that names concrete adapters, and the only place a
runtime value becomes a type.

## Use cases

A deployment runs one **use case**, selected by config:

```toml
use_case = "light-wallet"   # the default; the only variant today
```

A use case is a type in `zainod::use_case` binding three things the compiler
checks together:

- the **profile** it serves (`LightServeService`), carried as the `Serves<U>`
  bound;
- the **routing** — which provider answers each capability
  (`zaino_service::routing::LightRouting`);
- the **materialisation** — which indexes the finalised store builds
  (`zaino_indexes::sets::light_wallet::LightWallet`).

`indexer::select_use_case` is the one `match` over `UseCaseKind`; each arm
names a use case and the serving adapter that speaks its protocol, and nothing
else. `boot` is generic over the use case: the namespaces the backend opens,
the set the indexer builds, the type the store reader is wired to and the
routing the engine composes under all come from `U`, so they cannot be paired
wrongly. `use_case::compose` carries the single `demand ⊆ supply` check as its
`where` clause; a materialisation lacking an index the profile needs, or a
placement no provider can take, fails there.

Config **selects** a use case. It does not shape one: what is built on disk
and which provider answers a query are properties of the type, not knobs. See
the design note on the three roles of config for what a runtime placement knob
would cost.

## Adding a use case

1. A profile in `zaino-service` (a read-set plus controls), if none fits.
2. A routing type, one placement per varying capability.
3. A materialisation, one index list.
4. In `use_case.rs`: the marker type, its `UseCase` impl, and one
   `impl<S: TheProfile> Serves<TheUseCase> for S {}`.
5. A `UseCaseKind` variant and one arm in `select_use_case` naming the adapter.

Existing use cases do not change.
