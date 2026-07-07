# Spec: zainod learns regtest activation heights from the Validator

Solves zingolabs/zaino#1076. Requested by infras (`zingolabs/infrastructure#278`,
`bump_to_NU6.3`) in coordination with zaino's `ironwood_activation` e2e suite
(zingolabs/zaino#1368). Companion spec on the infras side:
`infras/dev/zaino-ironwood-activation-infra-spec.md` (devtool wallets on
arbitrary regtest heights — "Provider heights").

Governing invariant, decided 2026-07-06 (recorded as infras ADR 0003):

> **The Validator's configured activation heights are authoritative. Every
> other component mirrors them by the strongest mechanism its protocol
> allows.** No component may treat compiled-in or config-file regtest
> heights as chain truth.

Concretely, per component:

- **Indexer (this spec)**: queries the Validator's `getblockchaininfo` at
  startup and adopts the reported schedule.
- **Wallet client (infras side, same release as the guard lift)**: the
  light-client protocol doesn't expose the schedule, so the wallet can't
  ask the Validator itself — the harness queries `getblockchaininfo` on
  the wallet's behalf and writes the *derived* heights into the wallet's
  `activation-heights.toml`. The derivation is enforced at compile time:
  the wallet config's regtest variant demands an opaque
  `ValidatorHeights`, whose only public constructor is
  `WalletNetwork::from_validator(&validator)`. There is no
  caller-supplied-heights escape hatch; writing a height vector into a
  wallet config is unrepresentable.

Consequence for zaino's e2e suite: once both sides land, your
`ironwood_activation` fixture heights get typed in exactly one place — the
zebrad launch config. The wallet derives them via
`WalletNetwork::from_validator`; zainod adopts them via this spec; no
test-side constant needs to mirror anything. Expect the zcash_local_net
pin bump to be loud: `ZainodConfig.network` narrows to a payload-free
kind (Mainnet/Testnet/Regtest), `ZcashDevtoolConfig::faucet()/recipient()`
take a `WalletNetwork` parameter, and any test that assigned heights into
a wallet config stops compiling — the fix is always to launch the
validator first and derive.

## Problem

zainod's regtest activation heights are a compiled-in constant, silently
assumed to match whatever zebrad was launched with:

- The zainod TOML carries only the string `network = "Regtest"`.
  Deserialization inflates it to
  `Network::Regtest(ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS)`
  (`packages/zaino-common/src/config/network.rs:66`); the constant
  (`network.rs:13-26`) documents itself as "must equal zcash_local_net's
  `supported_regtest_activation_heights`" — a cross-repo mirror maintained
  by hand across three repos (zebra config, zaino constant, infras canonical
  heights).
- That `Network` flows into everything downstream as truth:
  `StateService::spawn` passes `config.common.network.to_zebra_network()`
  into `init_read_state_with_syncer` (`zaino-state/src/backends/state.rs:213-218`)
  and into `NodeBackedChainIndex::new` (`state.rs:267-276`);
  `FetchService::spawn` does the equivalent (`backends/fetch.rs:122+`).
- When the running zebrad's heights differ, block commitment verification
  computes against the wrong upgrade schedule and the chain-index sync loop
  dies with `InvalidData("Block commitment could not be computed")`
  (`zaino-state/src/chain_index/types/helpers.rs:183`,
  `block.commitment(network)`).

The mismatch is not hypothetical: infras is lifting its client-side guard so
devtool wallets run on **arbitrary** regtest heights, and zaino's own
`ironwood_activation` fixture (`ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS`)
puts NU6.3 at height 6 with everything else at 1–2. No compiled constant can
ever match caller-chosen heights. The only correct source is the running
zebrad — and it already publishes its schedule: `getblockchaininfo` returns
an `upgrades` map with per-upgrade `activationheight`, which zaino already
parses (`GetBlockchainInfoResponse.upgrades`,
`zaino-fetch/src/jsonrpsee/response.rs:356-359`, round-trip tested at
`response.rs:530+`). Both backends already call the validator at startup
(`get_info` at `state.rs:198`, `get_blockchain_info` in the tip-wait loop at
`state.rs:226`). The handshake exists; the heights are simply never adopted
from it.

## Requested change

1. **Learn heights at first contact.** When the configured network kind is
   Regtest, each backend fetches `getblockchaininfo` from the validator
   **before** constructing anything that consumes a
   `zebra_chain::parameters::Network` — the ReadStateService syncer, the
   chain index, the mempool source. Build the runtime regtest `Network` from
   the reported `upgrades` map. The RPC client is already constructed first
   (`state.rs:190-196`), so the ordering is achievable without restructuring.
2. **Learned heights are the only source.** `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS`
   ceases to exist as a truth source. Recommended shape: the *config-level*
   network type degrades to a kind (Mainnet / Testnet / Regtest, no payload),
   and only the *runtime* type constructed after the handshake carries
   heights — making pre-handshake height reads unrepresentable. A minimal
   variant (populate the existing payload from the handshake before first
   use) is acceptable if the type split is too invasive, but then nothing may
   read the payload before adoption. Behavior is mandated; shape is
   implementer's choice.
3. **An upgrade absent from the validator's map is never-activated.** This
   matches zebra's own semantics and the devtool's absent-key encoding.
   Do not backfill absent upgrades from any default.
4. **Failure is loud, never defaulted.** If the network kind is Regtest and
   the upgrades map cannot be fetched or parsed, startup fails with an error
   naming the validator endpoint. Under no circumstances fall back to a
   compiled or configured height set — a silently wrong schedule is the
   exact failure mode this spec removes.

## Acceptance

The fixture that must work end-to-end (as zebrad regtest config heights):

```
overwinter = 1, sapling = 1, blossom = 1, heartwood = 1, canopy = 1,
nu5 = 2, nu6 = 2, nu6_1 = 2, nu6_2 = 2, nu6_3 = 6
```

1. **Boundary sync**: launch zebrad regtest with the fixture; launch zainod
   against it with only `network = "Regtest"` in its TOML. Mine past height
   6. zainod's chain index syncs across the NU6.3 boundary with no
   commitment error, and serves compact blocks for both eras (Orchard
   coinbase at heights 2–5, Ironwood from 6).
2. **No-recompile proof**: the same zainod binary and config, pointed at a
   zebrad running a *different* schedule (the canonical all-at-2 set), also
   syncs — demonstrating heights come from the validator, not the build.
3. **Contract test on the upgrades map**: pin the `getblockchaininfo →
   heights` mapping with a test against real zebrad output (live regtest or
   a recorded golden response) — not hand-written assertions about what the
   RPC is believed to return. In particular, establish from the real output
   (not from reasoning) which upgrades zebrad includes in the map (e.g.
   whether anything pre-Overwinter appears) and encode exactly that.
4. **No residual truth source**: no non-test code path consumes
   `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS` or any other compiled regtest
   schedule as chain truth. Test-only fixtures that *are* their own chain
   (mockchain sources, proptest block generators) legitimately keep explicit
   heights — there, the test is the validator.

## Known unknowns to check while implementing

- **StateService init ordering**: `init_read_state_with_syncer` receives the
  network at construction and the ReadStateService validates blocks with it.
  Confirm nothing else needs a `Network` before the validator RPC is
  reachable; if something does, that is a design conflict to surface, not
  paper over.
- **zcashd as validator**: the legacy e2e path (`devtool_zcashd.rs`) drives
  zcashd, whose `getblockchaininfo` also reports an `upgrades` map. Prefer
  making the adoption path validator-generic so zcashd rides along; if its
  map shape differs materially, scope this spec to zebrad and record the
  divergence in the zcashd test docs.
- **zaino-testutils**: `live-tests/zaino-testutils/src/lib.rs:497` uses the
  constant to *launch zebrad* — that is legitimate harness-side
  configuration of the truth source, not adoption of it. Keep the height
  set, but move/rename it so it can no longer be mistaken for (or imported
  as) zainod's own schedule.
- **Reorg/restart**: heights are learned once at startup. A validator
  restarted mid-session with different heights invalidates the indexer's
  world; document that zainod must be restarted with its validator (regtest
  harnesses already do this), rather than attempting live re-adoption.

## Out of scope

- Mainnet / Testnet: compiled zebra network parameters are correct there; no
  handshake is needed and none should be added.
- The infras-side client guard lift (Provider heights) — lands on
  `infrastructure#278` independently.

## Delivery

A rev on a zaino branch that `live-tests` can exercise. Downstream effects
when it lands: zaino#1368's `ironwood_activation` tests gain a zainod that
can actually serve their fixture, and infras' nu6_3-at-6 integration test
(currently `#[ignore]`d naming #1076) flips live.
