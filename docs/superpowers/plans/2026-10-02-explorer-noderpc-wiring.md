# Explorer node-RPC: deployment + wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a `zainod` that serves the node-RPC use case over JSON-RPC against a
real validator, so the NightHawk explorer can run beside it. This is the bridge
from "the engine satisfies `NodeRpcService` in tests" to "a production engine
does, and a daemon serves it".

**User goal this serves:** the block-explorer use case served passthrough
first, then locally, **confirmed against the actual explorer. No PRs.**

**Spec:** `/home/chona/zingo/zingolabs/zaino-design/design/explorer-grant-completion.md`

## Where things stand (verified, branch `feat/noderpc-explorer`)

- The compile-time `assert_node_rpc` in `zaino-core/src/tests.rs` proves
  `EngineSnapshot`'s reads are complete for `NodeRpcReads` — over **mock tiers**
  and an **in-test** `NodeRpcRouting`.
- `zaino-runtime::boot::boot_indexed<D, A, C>` requires
  `IndexedEngine<D, C>: Serves<D::UseCase>` and takes a `serve` closure that
  returns the adapter. `JsonRpcServer` already implements `RunLoop`, as
  `GrpcServer` does. So a node-RPC deployment is mostly assembly.
- Every engine **control** (`TakeSnapshot`, `TipSubscribe`, `MempoolSubscribe`,
  `MempoolContent`, `Broadcast`, `NodeQueryRelay`) is implemented on `Engine`
  with plain `Src` bounds the production composite already meets.
- `zainod` serves exactly one thing: `DeploymentKind::LightWalletPassthrough` →
  `GrpcServer`. `DeploymentKind` is a deliberate closed set; `select_deployment`
  is bounded `C: LightWalletSource`.

## Gaps, in dependency order

1. **`SpendRead` in `NodeRpcReads`** cannot be satisfied in production:
   `SpendPlacement` has only a `Local` impl, and no real tier implements
   `SpendRead` on this branch (that is `feat/local-tier-reads`, unmerged).
   **User decision:** drop it now (no explorer method needs spend status);
   re-add with `gettxout`/`getspentinfo` after merging `feat/address-seam`.
2. **The production composite lacks the decoded-transaction port.**
   `ZebraValidator` (`zaino-source-zebra/src/routing.rs`) implements the block,
   header, verbose-block and blockchain-info ports, but not
   `OneShotGetTransactionVerbose` (only the zebra-rpc adapter and the mock do).
3. **No production `NodeRpcRouting`** — it exists only inside a test.
4. **No node-RPC deployment or source bundle** in `zaino-runtime`.
5. **`zainod` cannot select or serve it** — no dependency on `zaino-noderpc`,
   no `DeploymentKind` arm, no JSON-RPC bind address, no network mapping.

## Global Constraints

The slice-4 plan's Global Constraints apply unchanged (`unsafe_code`
forbidden; `missing_docs`; no `as` casts; no `.unwrap()` in production;
minimum visibility; typed error causes decided per site; `zaino-core` takes no
validator dependency and denies the wildcard lint; `HeightRange` inclusive;
`split_at_seam` untouched; every test fails if its behaviour is removed;
workspace-wide call-site search before any signature change; the shared-mock
six-crate gate; `--no-deps` clippy; commit forward, never amend, never stash;
**no attribution trailer; no PR; no push** — pushing is a separate, confirmed
step for the deploy).

Gate additions for this plan: `cargo test -p zaino-source-zebra`,
`cargo test -p zainod`, and the `zaino-runtime` integration tests.

---

### Task W1: Drop `SpendRead` from `NodeRpcReads`

User-directed. `zaino-service/src/read_sets.rs`: remove `SpendRead` from the
`NodeRpcReads` supertraits and blanket impl, and say in its doc why it is absent
and what restores it (served `gettxout`/`getspentinfo`, local spend status from
the tier work on `feat/address-seam`). In `zaino-noderpc`, delete the
now-unconstructed `RpcError::SpendRead` and its `to_error_object` arm (closes a
deferred Minor from Task 2b). The in-test `NodeRpcRouting` and
`assert_node_rpc` must still compile. Workspace-wide search first; full
shared-mock gate. One commit.

### Task W2: The composite source answers the decoded-transaction port

`ZebraValidator` implements `OneShotGetTransactionVerbose` by delegating to its
JSON-RPC adapter — **not** `state_then_fetch!`: the ReadState adapter has no
verbose form, and an unmined transaction is only found over RPC anyway. Read how
the composite delegates other RPC-only ports and follow that. Add a
compile-time assertion that `ValidatorClient<ZebraValidator>` (or whatever the
production client type is — find it in `zainod`) satisfies
`GetTransactionVerbose + GetBlock + GetBlockByHash + GetBlockchainInfo +
GetBlockHeader + GetBlockVerbose + GetTransaction`. One commit.

### Task W3: A production `NodeRpcRouting`

In `zaino-core/src/routing.rs`, beside `LightWalletRouting`:

```rust
pub struct NodeRpcRouting;
impl Routing for NodeRpcRouting {
    type Address = Passthrough;
    type Treestate = Passthrough;
    type Spend = Withheld;
    type TransactionLocation = Withheld;
}
```

Passthrough-first: every placement this use case reads is passthrough or
withheld; nothing depends on local tier reads this branch lacks. Doc it as the
passthrough deployment's routing, with the local one a sibling to come. Replace
the in-test `NodeRpcRouting` in `zaino-core/src/tests.rs` with this one so there
is one definition, and keep `assert_node_rpc` compiling against it. One commit.

### Task W4: The node-RPC deployment

`zaino-runtime/src/deployment/node_rpc_passthrough.rs`, mirroring
`light_wallet_passthrough.rs` exactly:

- `NodeRpcPassthrough: Deployment { UseCase = NodeRpc, Routing = NodeRpcRouting, Indexes = CompactBlocks }` and its `RuntimePlan` (sync-gated readiness, as the light-wallet one).
- `NodeRpcSource`: the hand-kept source bundle — `IndexedSource` plus every
  port the routing's passthrough placements and the always-passthrough reads
  relay through. Derive the list from the engine impls' `Src` bounds, not from
  memory; the light-wallet doc explains why it is hand-kept and that a missing
  port fails the demand bound at the wiring, naming it.
- **The real milestone, as a compile-time assertion:**
  `IndexedEngine<NodeRpcPassthrough, C>: Serves<NodeRpc>` for the production
  client type. Unlike `assert_node_rpc`, this one is over the real store and
  head tiers and the real source. If it fails, report the bound.

Then a `zaino-runtime` integration test booting it, following
`tests/full_boot.rs`. One or two commits.

### Task W5: `zainod` serves it

- Depend on `zaino-noderpc` (`cargo add`).
- `DeploymentKind::NodeRpcPassthrough` (kebab-case `node-rpc-passthrough`).
- `ServeConfig.jsonrpc_listen_address: SocketAddr`, defaulted. Respect the
  existing `allow_unencrypted_public_json_rpc_bind` feature flag's intent: a
  non-loopback bind without TLS should be refused unless that flag is set —
  read how the gRPC side handles the equivalent before deciding, and say what
  you did.
- Map the daemon's network to `zcash_protocol::consensus::Network` for the
  address-validation RPCs. Find the existing network type in `zainod`'s config
  and convert with a named function, not a cast.
- A `select_deployment` arm: `JsonRpcServer::new(NodeRpc::new(engine, network), jsonrpc)`.
  Widen the `C` bound to what both arms need.
- `zaino-noderpc` graduates from POC: write its `usage.md` and add it to the
  README index; do the crate-wide visibility pass (handlers `pub(crate)` where
  nothing outside the crate calls them). Both were deferred against exactly
  this trigger.
- A test that the daemon config parses `node-rpc-passthrough` and the bind.

Commits per bullet group.

---

## After this plan: the deploy (not a coding task)

The deploy needs the branch **pushed** (`deploy-ephemeral` takes a 40-char
ref). Pushing was pre-approved once this wiring exists, but **confirm with the
user before pushing**. Then, from `zaino-design`/devops notes:
`argo submit --from workflowtemplate/deploy-ephemeral -n argo -p namespace=<ns>
-p ref=<sha> -p zebra-tag=6.0.0 …` with the node-RPC deployment kind selected,
and the NightHawk explorer (`nighthawk-apps/zcash-explorer`, has a Dockerfile)
as a sidecar in the same namespace, pointed at `zaino:<jsonrpc port>` via
`ZCASHD_HOSTNAME`/`ZCASHD_PORT`. `zebra-tag=6.0.0` is mandatory: the golden
snapshot's zebra wedges two blocks short of Ironwood.

## Explorer must-haves that are NOT in this plan (tracked in slice 4)

From the client-strictness audit — the explorer crashes or silently fails
without these: `chainValue` floats in `valuePools`; `type` beside
`address_type` in `z_validateaddress`; `getinfo` emitting `build`; `getblock`
verbosity 0; `getblockhashes`. The deploy confirms which others the live
explorer needs.

## Deploy recipe (surveyed 2026-10-02; no chart change, no PR)

Verified against `zingolabs/devops` (`platform/argo-workflows/workflows/deploy-ephemeral.yaml`)
and the `zcash-stack` chart (`charts/zcash-stack`, cloned from **main** at deploy
time — so a chart change would mean a commit to that repo's main, which "no PRs"
rules out; none is needed):

1. **Select the deployment by env, not by chart.** `zaino-env` is a
   comma-separated `KEY=VALUE` list that becomes `--set zaino.extraEnv.KEY=VAL`.
   `zainod`'s config is layered with `ZAINO_`-prefixed env overriding TOML. Task
   W5 must therefore make `deployment = node-rpc-passthrough` and the JSON-RPC
   bind settable by env, and **report the exact env key names** (the nested-key
   separator in particular) so the deploy can set them, e.g.
   `-p zaino-env=ZAINO_DEPLOYMENT=node-rpc-passthrough,ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS=0.0.0.0:8232`.
   Bind `0.0.0.0` in-cluster requires W5's unencrypted-bind decision to permit it
   (feature flag or a cluster-internal allowance) — W5 must say how.

2. **The chart's zaino Service exposes only `grpc` (8137) and `metrics`.** No
   JSON-RPC port. Instead of changing the chart, apply a separate Service in the
   ephemeral namespace selecting the same pods:
   ```yaml
   apiVersion: v1
   kind: Service
   metadata: { name: zaino-jsonrpc }
   spec:
     selector: { app.kubernetes.io/name: zaino }
     ports: [{ name: jsonrpc, port: 8232, targetPort: 8232 }]
   ```

3. **The explorer runs beside it** as a Deployment in the same namespace:
   image built from `nighthawk-apps/zcash-explorer`'s `Dockerfile` with
   **podman** and pushed to the cluster registry; env
   `ZCASHD_HOSTNAME=zaino-jsonrpc`, `ZCASHD_PORT=8232`,
   `ZCASHD_USERNAME`/`ZCASHD_PASSWORD` (sent as HTTP basic auth, ignored by
   Zaino), `SECRET_KEY_BASE` (generate), `EXPLORER_HOSTNAME`, `EXPLORER_SCHEME`,
   `ZCASH_NETWORK=mainnet`, `VK_CPUS`/`VK_MEM` placeholders (the viewing-key
   feature is docker-in-docker and out of scope). **No Postgres**: the explorer's
   `ZcashExplorer.Repo` is commented out of its supervision tree.

4. **Deploy:** push the branch (confirm with the user first — a push, not a PR),
   then `argo submit --from workflowtemplate/deploy-ephemeral -n argo
   -p namespace=<ns> -p ref=<40-char sha> -p network=mainnet -p zebra-tag=6.0.0
   -p zaino-env=<as above>`. `zebra-tag=6.0.0` is mandatory (the golden snapshot's
   zebra wedges two blocks short of Ironwood at 3,428,141).

5. **Confirm:** port-forward the explorer and walk its pages — home (metrics
   warmers), recent blocks, a block, a transaction, an address, the search box
   with a block height, a txid, a t-address, a Sapling and a unified address —
   reading the explorer's own logs for `FunctionClauseError`/`MatchError`.
   Also probe the 17 methods directly against `zaino-jsonrpc:8232` with curl.
   Watch search latency: it gives its four parallel probes 5 s, and a timeout
   crashes the page.
