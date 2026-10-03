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
address history relayed to the validator (`LightWalletPassthrough`) and with a
local transparent index later are two
deployments of one use case. Routing and index set are the deployment's
choices, not the use case's, so a second implementation of the same demand is
free to make them differently.

The node-RPC / block-explorer demand has one deployment so far,
`NodeRpcPassthrough`: the `TransparentHistory` index set (compact blocks plus the
address-history, transparent-spends and txid-location indexes), address history
and spend lookup both placed `Local` and every other placement relayed to the
validator or withheld (`NodeRpcRouting`), and the `NodeRpcSource` bundle. It
serves `getaddressdeltas` and `getspentinfo` from its own indexes, which a
plain-RPC validator cannot answer, so `NodeRpcSource` names no address or spend
source port. (Its name predates the local address flip and is now a misnomer — a
rename is deferred because it is the kebab-case deployment value in the live
`zainod` config.)

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
repairs a watermark the headers index does not bear out, checks the store's
index coverage (below), resumes the indexer, anchors the chain head over the
same client, composes the engine and boots the lot under one `Orchestra` in
dependency order — validator, indexer, store, chain head, then the server
`serve` builds over the engine. The caller supplies the client (it built the
validator, so it knows it is reachable) and the serving adapter (the runtime
is protocol-agnostic), and runs the returned `Orchestra` until a signal or an
escalation.

### Boot refusal: an index set grown over an existing store

`boot_indexed` refuses to start — `DeployError::IndexCoverage`, printed and
exit 1 from `zainod` — when the deployment declares an index the data
directory's store never built, and the store has already committed a
watermark. This is the state a deployment change leaves when it gains an index
(for example moving from the compact-block set to `TransparentHistory`, which
adds the address-history, transparent-spends and txid-location indexes) and
points at a data directory an earlier deployment already synced: the new index
would resume from the shared watermark and cover only `[resume, tip]`, then
report serviceable while silently missing all history below the resume height.
The error names the unstamped index(es) and the watermark. **Remedy: point the
deployment at a fresh, empty data directory and resync from genesis.** An empty
data directory, or one whose every declared index is already built, boots
normally; dropping an index (reopening with a subset) is always safe. The guard
itself lives in `zaino-store` (see its guide).

## Supervision

The runtime is the conductor. `OrchestraBuilder` boots components in order
(each `Ready` before the next), gives each a babysitter (`supervise`, acting
per a `RecoveryPolicy`), and funnels escalations up one channel.
`Orchestra::run` turns the first escalation into a `RuntimeOutcome`;
`Orchestra::signals` projects the components' statuses onto the three
cloud-native probes, judged by the plan's `ReadinessCriteria`, and
`HealthServer` serves them over HTTP.
