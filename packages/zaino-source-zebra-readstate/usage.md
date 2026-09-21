# `zaino-source-zebra-readstate` — usage

The read-state adapter: implements `zaino-source` ports by reading Zebra's
state database directly, with no RPC round trip.

```rust
use zaino_source_zebra_readstate::ZebraReadStateAdapter;

let adapter = ZebraReadStateAdapter::new(read_state_service, network);
```

Requires Zaino and Zebra to be on the same host, sharing the database
directory. Deployments without that run RPC-only, and everything still works —
this adapter is an **accelerator, not an alternative**.

## What it implements, and what it deliberately does not

It implements 22 ports: block and header reads, chain tip, address queries,
treestate and commitment trees, subtree roots, transactions, block deltas,
blockchain info, and the block/tip subscriptions.

It does **not** implement the mempool ports, the passthrough ports
(`getpeerinfo`, `getmininginfo`, `getnetworksolps`, …), `GetAddressDeltas`, or
`SubscribeChainTip`. That is the capability model working, not a gap to fill:

- A state service **has no mempool**. An implementation here could not see
  unconfirmed transactions, so it would silently answer a different question
  than the one asked. ADR-0008's structural-capability rule means routing a
  mempool query here is a compile error rather than a wrong answer.
- `GetAddressDeltas` is a *composition* — address txids, then every
  transaction, then a derivation — and the transaction step needs the mempool.
  It belongs above the adapters, where both transports are in reach.

**Do not add a partial implementation of a port to "cover more cases".** A port
that answers 90% of the question is worse than one that does not exist, because
the composite can no longer tell that it needs the other transport.

## `GetBlockDeltas` is the exception worth knowing about

`getblockdeltas` was once on the not-implemented list, on the reasoning that it
avoided "a second copy of logic the validator already has, for no capability
gain". That reasoning was **false**: zebrad does not implement `getblockdeltas`
at all. The derivation here is the only implementation a zebrad-backed
deployment has, and the composite routes to it *first* for exactly that reason.

The lesson generalises. Before declining to implement a port here on the grounds
that the validator already does it, check that the validator actually does.

## Derivations live here, and are tested here

Some answers are computed rather than read: `median_time_past` over an
11-block window, difficulty as a ratio against the network's target limit,
prevout resolution for block deltas. These have unit tests in the adapter
because they are the parts that can be silently wrong — a difficulty off by a
shift produces a plausible number.

## Network upgrade names

Take the wire name from serde, not from `Debug`:

```rust
serde_json::to_value(upgrade)   // "Nu5"  — the wire name
format!("{upgrade:?}")          // "NU5"  — not the wire name
```

Zebra's `Display` is its `Debug`, and neither matches what the RPC surface
emits; the names come from the enum's serde renames. This produced a live-suite
failure where the RPC path (which takes the name from the validator's reply) and
this path disagreed on the same upgrade.

## Related

- ADR-0008 — structural capability, and why partial ports are not allowed.
- `zaino-source-zebra` — the composite that routes between this and RPC.
