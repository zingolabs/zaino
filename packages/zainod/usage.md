# zainod

The daemon: the composition root that turns a `DaemonConfig` into a running
Zaino. It is the only crate that names concrete validator adapters and
serving adapters, and the only place a runtime value becomes a type.

## Deployments

A daemon runs one **deployment**, selected by config:

```toml
deployment = "light-wallet-passthrough"   # the default
# deployment = "node-rpc-passthrough"     # the node / block-explorer JSON-RPC
```

| Deployment | Serves | Listens on |
|---|---|---|
| `light-wallet-passthrough` | `CompactTxStreamer` gRPC | `serve.grpc_listen_address` (default `127.0.0.1:8137`) |
| `node-rpc-passthrough` | Zcash node JSON-RPC (zcashd-compatible) | `serve.jsonrpc_listen_address` (default `127.0.0.1:8232`) |

Both are layered from `ZAINO_`-prefixed env with `__` for nesting, e.g.
`ZAINO_DEPLOYMENT=node-rpc-passthrough` and
`ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS=0.0.0.0:8232`. A non-loopback bind is
taken as given: neither server applies TLS or an authentication layer, so a
public bind belongs behind a boundary the deployment controls.

A deployment is a type in `zaino_runtime::deployment` binding the use case it
serves, the routing the engine is composed under and the index set the store
builds; see the runtime's guide. `indexer::select_deployment` is the one
`match` over `DeploymentKind`: each arm names a deployment and the serving
adapter that speaks its use case's protocol, hands both to the runtime's
`boot_indexed`, and nothing else. Adding a deployment is adding an arm; the
compiler checks the arm's shape at the runtime's `compose`.

Config **selects** a deployment. It does not shape one: what is built on disk
and which provider answers a query are properties of the type, not knobs. See
the design note on the three roles of config for what a runtime placement knob
would cost.

## Validators

The daemon builds one `ValidatorClient` over one shared validator adapter and
hands that same client to the runtime, which hands it to every consumer — the
indexer, the chain head, the engine's passthrough — so nothing above the
client touches a single-attempt port and retrying happens in exactly one
place.

`[source]` in config selects the adapter and its transport; today both arms
build a Zebra adapter. A new adapter — a zcashd-contract JSON-RPC one, say —
implements the `OneShot*` ports it can, and the compiler says which
deployments the client over it can serve: an arm whose deployment's `…Source`
bundle names a port the adapter lacks does not compile.

`[indexer] fetch` pairs with the source: `compact` (default) needs a validator
serving zaino's pre-index compact read, i.e. the zebra fork over either
transport; `full` indexes from whole blocks over the standard read, so an
`rpc` source pointed at a stock validator works, at the cost of deserialising
proofs the indexes never read. The mainnet RPC fixture takes the same choice
from `ZAINO_TEST_FETCH`.

## Boot-time checks

Two things happen before the indexer resumes, both loud when they fire:

- **Watermark repair** (the runtime's, in `boot_indexed`). The store's
  watermark is a stamp beside the data. If it claims a height the headers
  index does not hold, every height in the gap would be routed to the store
  and answered "no such block", and the indexer would resume past heights it
  never indexed. The runtime re-stamps it at the highest header held and logs
  a warning naming both heights.
- **Tip polling** (the daemon's). Neither Zebra transport pushes tip changes, so the daemon
  hands the validator a second RPC handle to poll every two seconds. Without
  it the finalised indexer's follow loop has nothing to wake it and the index
  stays at its catch-up height; the driver warns if it is ever handed a source
  it cannot follow.

