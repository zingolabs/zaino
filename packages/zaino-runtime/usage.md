# zaino-runtime

Two halves: the **deployments** — how each use case is served — and the
**supervision** they run under.

## Deployments

A use case (`zaino_service::use_cases`) names what a consumer demands. A
deployment names how this runtime meets it. `deployment::Deployment` is a
marker type with three associated types the compiler checks together:

- `UseCase` — the demand it meets, carried as the `Serves<U>` bound;
- `Routing` — which provider answers each capability
  (`zaino_core::routing`);
- `Indexes` — which indexes the finalised store builds
  (`zaino_indexes::index_set`).

`deployment::compose` builds the engine for a deployment and carries the one
`demand ⊆ supply` check as its `where` clause: an index set lacking an index
the demand needs, or a placement no provider can take, fails there, at the one
call site per deployment, rather than at a request.

```text
wired(D) = Engine<StoreReader<B, D::Indexes>, Nfs, Src, D::Routing>
ok(D)    ⟺ wired(D): Serves<D::UseCase>
```

One use case, several deployments: the light-wallet demand served with
address history relayed to the validator (`LightWalletPassthrough`, the only
deployment today) and with a local transparent index later are two
deployments of one use case. Routing and index set are the deployment's
choices, not the use case's, so a second implementation of the same demand is
free to make them differently.

Each deployment is one file under `deployment/` holding its marker, its
`Deployment` impl, its `RuntimePlan` impl and the validator bundle it
requires (`LightWalletSource`: the indexed assembly's floor,
`IndexedSource`, plus every port its passthrough placements relay through).
A deployment that outgrows a file moves to a crate of its own without
changing anything above it; the seam is these traits, not a crate boundary.

## Runtime plans

`RuntimePlan` is the runtime side of a deployment: the typed config sections
its assembly consumes (`Config`) and what its readiness gates on
(`READINESS`). A deployment with a local index takes
`config::IndexedDeploymentConfig` (`StoreConfig` + `IndexerConfig`) and gates
readiness on the indexer's sync; one that serves only by passthrough would
take no store and not gate on a sync it never runs.

`IndexerConfig::fetch` picks what the indexer asks the validator for at each
height. `full` is the default: the whole block over the standard read, which
any validator answers, projected by walking the encoding so nothing the
indexes discard is deserialised. `compact` is the pre-index compact block,
the same fields skipped on the validator's side instead of ours, which only
zaino's zebra fork serves. Both project to the same provisioning context, so
the index built is identical either way.

`boot_indexed::<D, _, _>(client, &config, serve)` is the indexed assembly:
it opens the LMDB store with exactly the namespaces `D::Indexes` writes,
repairs a watermark the headers index does not bear out, resumes the indexer,
anchors the chain head over the same client, composes the engine and boots
the lot under one `Orchestra` in dependency order — validator, indexer,
store, chain head, then the server `serve` builds over the engine. The caller
supplies the client (it built the validator, so it knows it is reachable) and
the serving adapter (the runtime is protocol-agnostic), and runs the
returned `Orchestra` until a signal or an escalation.

## Supervision

The runtime is the conductor. `OrchestraBuilder` boots components in order
(each `Ready` before the next), gives each a babysitter (`supervise`, acting
per a `RecoveryPolicy`), and funnels escalations up one channel.
`Orchestra::run` turns the first escalation into a `RuntimeOutcome`;
`Orchestra::signals` projects the components' statuses onto the three
cloud-native probes, judged by the plan's `ReadinessCriteria`, and
`HealthServer` serves them over HTTP.
