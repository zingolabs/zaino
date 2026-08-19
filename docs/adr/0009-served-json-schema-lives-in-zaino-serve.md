# The served JSON schema lives in `zaino-serve`, beside its only consumer

## Status

accepted

## Context and decision

`zaino-fetch`'s `jsonrpsee/response/**` — roughly 5,500 lines — was
**dual-purpose**, and that is what kept the crate alive long after its
transport was replaced. Those types deserialized *validator replies* and
serialized *Zaino's own JSON-RPC server replies*. One set of structs, two
opposite roles, on opposite sides of the indexer.

The new source stack superseded the first role completely and the second not at
all. `ZcashIndexer` returned those types from 14 methods; that, and only that,
is what pinned `zaino-fetch` in the workspace after `zaino-rpc` and
`zaino-source-zebra-rpc/parse.rs` had taken over inbound parsing.

Serving one type in both directions is not a tidiness problem. It means the
shape Zaino *accepts* from a validator and the shape Zaino *emits* to a client
cannot diverge, even where the interfaces genuinely differ — and they do
differ. zebrad spells a value pool `lockbox` where zcashd spells it `deferred`;
`z_gettreestate`'s `finalRoot` is display-order while `z_getsubtreesbyindex`'s
subtree roots are not. A single struct forces one answer to questions that have
two.

We decide:

1. **The served JSON schema is `zaino-serve`'s**, in
   `src/rpc/jsonrpc/wire/` — serde structs carrying zcashd's exact field names,
   one module per response family, next to the `#[rpc(server)]` trait that is
   their only consumer.

2. **Conversion is one direction and one function**: `from_domain(domain) ->
   Wire`, infallible where the wire type can express everything the domain type
   holds. `BlockDeltas::from_domain` is the single exception, returning
   `Result<_, DeltaAmountOutOfRange>` because the interface's amount type is
   narrower than the domain's.

3. **`ZcashIndexer` returns domain types.** All 25 non-proto returns, not just
   the 14 that pinned `zaino-fetch` — including those that returned
   `zebra_rpc::methods::*`, which is what removes `zebra-rpc` from most of
   `zaino-state`'s public API.

4. **Golden tests travel with the type.** A field-name change here is a wire
   break, so each wire module carries its own serialization tests in the same
   file, asserting against literal `json!` values rather than round-tripping
   through the type that produced them.

5. **gRPC stays out of scope.** `zaino_proto::proto::*` returns are machine
   generated from the canonical `.proto`, single-sourced, and pin nothing.

### Naming: `from_domain`, not `to_wire`

CLAUDE.md's wire-boundary doctrine specifies `to_wire` / `try_from_wire` as
inherent methods on the *business* type. That shape is not available here: the
business types live in `zaino-primitives`, whose only dependency is
`thiserror`, and giving them a `to_wire` method would mean giving that crate a
serde dependency and knowledge of the serving layer — the exact coupling
ADR-0008 exists to prevent.

So the conversion lives on the wire type instead, named `from_domain`. The
doctrine's *purpose* is preserved: the direction is in the name, the call site
reads unambiguously, and there is no `impl From` hiding a boundary crossing
behind `.into()`. The `lint-boundary-conversions` check still forbids
`impl From`/`TryFrom` across this boundary.

There is a genuine asymmetry with the inbound direction, and it is not
accidental. Inbound (`zaino-source-zebra-rpc/parse.rs`) is *external-input
validation* and is fallible by nature. Outbound is rendering something Zaino
already vouches for, and is infallible almost everywhere. Different names for
different contracts.

## Considered options

- **Keep the dual-purpose types in a shared crate** — rejected: it is the
  status quo that pinned `zaino-fetch`, and it structurally prevents the two
  directions from disagreeing where the interfaces do.

- **Put the wire types in `zaino-primitives` behind a `serde` feature** —
  rejected: a feature-gated serde impl is still a schema decision made in the
  domain crate, and feature unification means one consumer enabling it enables
  it for all. The property that makes `zaino-primitives` depend on nothing is
  worth more than the deduplication.

- **Generate the served schema from a specification** — deferred, not rejected.
  It is the right long-term answer for a wire contract, and `zaino-proto`
  already works this way for gRPC. There is no machine-readable specification of
  the zcashd JSON-RPC surface to generate from, and writing one is a larger
  piece of work than this rewire.

## Consequences

- **A defect closed rather than carried.** `zaino-serve`'s error recovery
  downcast-walked for `zaino_fetch`'s `RpcError`, which the new stack never
  constructs — so zcashd error-code recovery was **silently inert**: every code
  reached the client as a generic internal error. It now matches
  `zaino_source::FetchError`'s `FailureMode::RpcError(i64)` (the validator's
  code) and `zaino_state::LegacyRpcError` (Zaino's own), and is tested
  directly rather than only from the far side.

- **Interface asymmetries are now recorded where they are served.**
  `wire/treestate.rs` states that `finalRoot` is display-order and that
  `wire/subtrees.rs`'s roots are not, with a test pinning each. Previously the
  reversal happened somewhere in a shared type and neither call site said so.

- **Both spellings of a value pool are accepted.** `lockbox` (zebrad) and
  `deferred` (zcashd) map to the same domain pool, so the served answer does not
  depend on which validator is behind the adapter. This was a live-suite
  failure, not a hypothetical.

- **`zaino-serve` grew from 2 unit tests to ~119.** The wire modules are the
  only place the served field names are asserted, so that is where the coverage
  had to go.

## Related

- ADR-0008 — validator access is a set of single-question ports over domain
  primitives. This ADR is its outbound half.
