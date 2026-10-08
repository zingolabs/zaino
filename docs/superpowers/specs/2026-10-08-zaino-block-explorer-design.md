# zaino-block-explorer: design

## Purpose

A minimal, honest terminal (and later, web) client for zaino's node-rpc
surface, built to run daily against our own zainod deployment. The point is
dogfooding the exact wire contract a real explorer backend consumer hits —
not a polished product, and explicitly not a competitor to NightHawk or
CipherScan. It doubles as the reference client for the node-rpc methods
currently in flight (`getchaintips`, `getblocksubsidy`, `gettxoutsetinfo`),
the same role NightHawk played for Zebra's own explorer epic (zebra#8435).

Non-goal, stated up front: this does not replace the separate, still-parked
OpenRPC/generated-wire-types project (`project_zcash_rpc_spec_crates.md`).
That project is implementation-agnostic — a spec other languages and other
implementations could target. What this spec builds is implementation-
*coupled*: a client generated from zaino's own Rust trait definition. Useful
today, not a substitute for the standardization project.

## Interface mechanism: jsonrpsee-generated client

`zaino-noderpc`'s entire RPC surface is already one trait,
`src/rpc.rs:37-38`:

```rust
#[rpc(server)]
pub(crate) trait NodeRpcApi {
    #[method(name = "getblockcount")]
    async fn block_count(&self) -> Result<u32, ErrorObjectOwned>;
    // ... one method per RPC, real param/return types throughout
}
```

jsonrpsee's own macro generates a fully typed async client from the same
trait definition when the attribute is `#[rpc(client, server)]` — no
hand-written transport, no separate schema, no codegen pipeline. This
requires three small, mechanical changes to `zaino-noderpc`, independent of
the explorer binary itself:

1. `NodeRpcApi` trait: `pub(crate)` → `pub`, `#[rpc(server)]` →
   `#[rpc(client, server)]`.
2. Wire DTOs (`wire/response.rs`, `wire/params.rs`) are already `pub`, but
   one-directional: responses derive `Serialize` only, params derive
   `Deserialize` only. A client needs the opposite on each — add the missing
   derive to both families. This changes no wire shape, only which
   directions serde supports.
3. Our own client needs none of the `ZcashdDialectLayer` bridge
   (`src/transport/dialect.rs`) — that exists only to accommodate external
   zcashd-1.0-dialect callers like `zcashex`. A jsonrpsee client speaks
   standard JSON-RPC 2.0 natively; the dialect layer is a server-side
   concern this client never touches.

**This widening is independently useful and independently small.** It should
land back on `feat/noderpc-explorer` on its own as soon as it's done, rather
than sitting only on this branch — we just finished auditing six sibling
branches that drifted independently off that same base for weeks; this is
exactly the kind of small, mergeable, non-explorer-specific change that
shouldn't repeat that pattern.

## Crate layout

New workspace member `packages/zaino-block-explorer/`, with two binary
targets sharing one dependency on `zaino-noderpc`'s generated client — no
other zaino-internal crate dependency, no domain-type reconstruction, no
caching or secondary index of its own. Every view is a live RPC call; that's
the whole point, and the opposite of CipherScan's bolted-on Postgres layer.

- `bin/tui.rs` — the primary surface. `ratatui` + `crossterm`.
- `bin/web.rs` — a secondary surface, deferred to Phase 2+. Server-rendered
  with `axum` + a plain templating crate (Askama or `maud`), using the exact
  same native client unmodified — no WASM, no frontend build pipeline. This
  is architecturally the closest match to NightHawk's own Elixir/Phoenix
  server-rendered design. A WASM/SPA frontend (Yew/Leptos/Dioxus, which
  jsonrpsee also supports via its web-sys/fetch transport) is a real option
  if a client-side app is ever wanted, but out of scope for this spike.

Config: one RPC endpoint URL, CLI flag or env var, nothing fancier.

## Screens / views

TUI, smallest useful set: home/status (chain height, mempool size), block
list → block detail, transaction detail, address view (balance/deltas/
txids — exercises the local-served methods), a search bar routing a
hash/height/txid/address to the right view. Two screens are explicitly
deferred until their RPC exists: chain-tips view and UTXO-set-stats view.

The web surface mirrors the same views once built; no screen exists on one
surface and not the other.

## Build phases

- **Phase 0 — client + skeleton.** The `zaino-noderpc` widening above, then
  crate scaffold + static home screen against whatever's already live
  today. No new backend work needed; fastest path to something running
  daily.
- **Phase 1 — block/tx/address views.** Exercises the full currently-served
  surface.
- **Phase 2 — chain-tips view**, once `getchaintips`/`getblocksubsidy` land
  (the slice work discussed separately).
- **Phase 3 — UTXO-set view**, once the Fold-bridge toy index +
  `gettxoutsetinfo` land.
- **Phase 4 — web surface**, server-rendered, reusing the Phase 0-3 client
  and view logic. Not gated on the TUI being "done" — can start once Phase 1
  lands, same as the TUI did.

## Testing

A handful of integration tests against zaino-testutils' existing
local-harness pattern, checking each view's RPC calls parse without
panicking. TUI/web rendering itself untested — standard for both, not worth
the effort for a dogfood spike.

## Explicit non-goals

No caching/persistence of its own, no auth, no multi-node support, no
mempool streaming, no visual polish, no packaging/release pipeline, no
shielded-balance anything (a generic explorer cannot show shielded balances
without a viewing key — see `project_be_outreach_scouting.md`). A public
aggregate shielded-pool dashboard (total shielded value over time, pool
migration stats) was floated separately as a real differentiator idea but
needs RPC support that doesn't exist yet — explicitly out of scope here.

## Where it lands

New worktree + branch `feat/block-explorer`, off `feat/noderpc-explorer`
(created 2026-10-08, sibling directory
`/home/chona/zingo/zingolabs/zaino/block-explorer`, matching this repo's
existing sibling-worktree convention). Loose/untracked, per explicit
decision — this is a side/dogfood project, not core roadmap work.
