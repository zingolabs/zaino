# Ironwood activation: test design and heights source-of-truth roadmap

State capture, 2026-07-06. Design session artifacts live in this tree
(uncommitted at time of writing); coordination artifacts live in
`zingolabs/infrastructure` (PR #278 worktree). This note is the durable
record of the decisions and the facts they rest on.

## Domain facts, with sources

- **Ironwood is a new shielded pool** activated by NU6.3
  (<https://zcash.github.io/ironwood/>). It shares the Action-circuit shape
  with Orchard but is a distinct pool.
- **Public testnet activated NU6.3 at height 4,134,000, ~2026-07-04.**
  Defined identically in zebra-chain 11.0.0
  (`parameters/constants.rs::activation_heights::testnet::NU6_3`) and
  zcash_protocol 0.10.0-pre.0 (`consensus.rs`), consensus branch ID
  `0x37a5165b` in both. Wallet stack and validator stack therefore flip eras
  at the same height with no drift. **Mainnet has no NU6.3 height** in those
  pins. (Gotcha: public explorers can mislabel testnet/mainnet heights —
  verify against a synced node.)
- **ZIP 318 (zcash/zips PR #1317), "Orchard to Ironwood migration"**: wallet
  best-practices for a two-phase scheduled migration. Normative anchor: "The
  user MUST be able to migrate Orchard-pool funds to the Ironwood pool, and
  MUST be informed that doing so is necessary to retain access to those
  funds." The wallet-visible migration transaction is an Orchard spend with
  Ironwood outputs.
- **Cross-address restriction**
  (<https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>):
  post-NU6.3 the Orchard Action circuit requires "(g_d, pk_d) of the output
  note must equal (g_d, pk_d) of the spent note" — every Orchard action is
  either change to the spent note's own address or a withdrawal (positive
  value balance). Cross-address Orchard transfers are circuit-impossible,
  including between two addresses of one wallet. A companion
  transaction-level rule forbids new value entering the pool.
  - Do **not** describe Orchard as "exit-only": same-receiver change still
    lands in the pool, so the Orchard note-commitment tree keeps growing
    after activation. There is **no frozen-finalRoot predicate**; the
    correct chain-walk predicate is **Orchard pool value non-increasing from
    the boundary** (observable via `valuePools`).
- **No Orchard TAZ is obtainable** (and the testnet pre-epoch is closed):
  post-activation nothing new can enter Orchard, so pre-activation Orchard
  notes exist only in wallets that held them before the flip. Zaino holds
  none. Consequence: the migration cell is exercisable **only on hermetic
  regtest transition chains** — permanently.

## Language decisions (canonical in CONTEXT.md)

- **Testnet** names only the public test network. Every hermetic local net
  is a **regtest net**, whatever network-kind flag it runs under.
- **Pool names pair with pool names** (Orchard ↔ Ironwood); **NU6.3** names
  only the upgrade itself (activation heights, branch ID, consensus rules).
  Applied renames: `NU6_3_ACTIVE_ACTIVATION_HEIGHTS` →
  `IRONWOOD_ONLY_ACTIVATION_HEIGHTS` (zaino-testutils),
  `NU6_3_ACTIVE_HEIGHTS` → `IRONWOOD_ONLY_HEIGHTS`
  (`proptest_blockgen.rs`). `NU6_3_TRANSITION_BOUNDARY` keeps the upgrade
  vocabulary: an activation height is the upgrade's concept.

## Design model: predicates × regimes

A **regime** is a (network, era) pair; a **predicate** is a
wallet-observable claim whose truth is fixed per regime. The suite is the
truth table; tests instantiate predicates in every reachable regime.

| predicate | Orchard era | Ironwood era |
|---|---|---|
| unified-address receipt lands in Orchard | true | false |
| unified-address receipt lands in Ironwood | false (pool inactive) | true |
| shielded-receiver coinbase pays the era pool | Orchard | Ironwood |
| Orchard note spends into an Ironwood receipt (ZIP 318 migration) | n/a | true |
| `shield` deposits into the era pool | Orchard | Ironwood |
| anything can land in Orchard from outside | true | false (no-new-value rule) |
| Orchard pool value non-increasing | n/a | true (cross-address restriction) |

Regime venues:

- **Regtest, Ironwood-only** (`IRONWOOD_ONLY_ACTIVATION_HEIGHTS`, canonical
  NU6.3-at-2): the existing devtool wallet tests (`send_to_ironwood`,
  `shield_for_validator`, `receives_mining_reward`, `send_to_all` in
  `live-tests/e2e/tests/devtool.rs`).
- **Regtest, Orchard-only** (`ORCHARD_ONLY_ACTIVATION_HEIGHTS`): clientless
  / wire mining fixtures only (devtool wallet cannot run there today).
- **Regtest, transition** (`ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS`,
  NU6.3 at `NU6_3_TRANSITION_BOUNDARY` = 6): coinbase-routing and era
  composition covered clientless
  (`clientless/tests/compact_block_consistency.rs`) and over the wire
  (`e2e/tests/compact_block_wire.rs`); **wallet cells live in
  `e2e/tests/ironwood_activation.rs`** (see below).
- **Public testnet**: observational boundary walk across height 4,134,000 —
  durable forever, historical blocks don't expire; third-party ZIP 318
  migrations supply the Orchard-spend traffic zaino must index but cannot
  author. Active cells post-flip only, Orchard-free: faucet TAZ →
  transparent receipt → `shield` → Ironwood send/receive. Env-gated manual
  runs; record txids/heights in a dated evidence log
  (`docs/notes/ironwood-activation-evidence.md`, created on first run).
  `ZEBRAD_TESTNET_CACHE_DIR` (zaino-testutils) anticipates the cached sync.

## Landed artifacts (this tree)

- **`live-tests/e2e/tests/ironwood_activation.rs`** — the transition-regime
  wallet cells, full real bodies, gated
  `#[should_panic(expected = "UnsupportedActivationHeights")]`:
  - `unified_receipt_lands_in_orchard_before_boundary`
  - `orchard_note_spends_to_ironwood_across_boundary` — the migration cell;
    recipient unified address generated *after* activation; asserts the
    faucet's Orchard balance strictly shrinks, guarding against devtool note
    selection dodging the migration by spending the boundary-height Ironwood
    coinbase instead (sound under the cross-address restriction: change
    returns only to the spent note's receiver, so a genuine Orchard spend
    nets sent-plus-fee out of the pool).
  - Verified: both pass as should_panic in ~5 s each
    (`cargo nextest run -p e2e -E 'binary(ironwood_activation)'`).
  - Registered on one backend (FetchService) while known-red: the panic
    fires before the backend is exercised; mirror the fetch/state matrix
    when the cells go live.
- **`e2e::devtool::build_clients_at(port, &heights)`** — devtool wallets on
  caller-chosen regtest heights; `build_clients` delegates with the
  canonical set.
- CONTEXT.md glossary entries; the renames above.

## The blocker, and the infra handoff

`zcash_local_net`'s devtool client rejects every regtest heights set except
its canonical NU6.3-at-2 one at wallet launch
(`ClientError::UnsupportedActivationHeights`; the guard exists because
heights drift between wallet and validator produces "incorrect consensus
branch id" — see zaino#1368). The guard predates the current devtool, which
reads heights at `init` from the `--activation-heights` TOML the client
already writes.

Handoff: **`~/src/zingolabs/infras/dev/zaino-ironwood-activation-infra-spec.md`**
(for zingolabs/infrastructure PR #278, branch `bump_to_NU6.3`; zaino pins
rev `0dc4a51f...`). Asks: lift the equality guard (canonical set stays the
default), retire the error variant (zaino's known-red tests key on its name
so the pin bump flips them loudly), keep the caller-responsibility doc
contract, plus an infra-side integration test proving era-correct scanning
and branch-ID selection from file heights. Known unknown flagged: devtool
note-selection policy across pools.

Scope addition (confirmed): infra also narrows `ZainodConfig.network` from
`NetworkType` to a payload-free `NetworkKind` in the same release — the
heights payload there is dead code (discarded at
`network_type_to_string()`; the Indexer never received heights that way).
Validator configs keep full `NetworkType`: they configure the source of
truth.

## Roadmap: source of truth first

Current directive (supersedes "unblock the suite next"): **first** make the
Validator the single source of truth for activation heights, with
regression tests that keep that source unique, and fix zaino#1076.

Invariant: *the Indexer learns activation heights from the Validator at
startup and accepts them from nowhere else; configs carry network kind
only.*

Audit facts grounding the implementation:

- zainod's TOML is already kind-only: `NetworkSerde` serializes
  "Mainnet"/"Testnet"/"Regtest", and deserializing "Regtest" injects the
  compiled-in `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS`
  (`zaino-common/src/config/network.rs`). The real second channel is the
  library seam: `Network::Regtest(ActivationHeights)` constructed
  programmatically (e.g. by `TestManager`) and consumed via
  `config.get_network()` → `to_zebra_network()`.
- The validator channel already exists in zaino's types:
  `GetBlockchainInfoResponse.upgrades` is parsed (zaino-fetch
  `jsonrpsee/response.rs`) and consumed (`backends/fetch.rs`,
  `latest_network_upgrade`). The work is deriving the runtime
  `zebra_chain::parameters::Network` from that response at startup and
  narrowing the config type to kind-only.
- Regression tests that maintain uniqueness: launch the validator at
  non-canonical heights (the transition fixture) with a kind-only-configured
  zainod and assert era-correct serving (fails if anyone re-couples zainod
  to a compiled-in set); the narrowed config type itself is the compile-time
  regression test; a serde test pins the TOML shape.
- Payoff: the `InvalidData("Block commitment could not be computed")`
  config-drift failure class becomes structurally impossible — the same
  invariant the infra guard lift expresses on the wallet side (wallets get
  heights from a file matched to the validator; the indexer gets them from
  the validator directly).
- **#1076 residue** (issue text predates the all-at-2 canonical set, which
  already replaced the NU6.1-at-1000 sentinel): wire
  `lockbox_disbursements` into zebrad-launching fixtures (plumbing landed
  at pinned rev, infra#243); add one NU6.1 boundary-crossing test (same
  transition-fixture pattern as the Ironwood suite); watch for
  block-commitment regressions.

## Sequencing

1. Zaino: heights source-of-truth implementation + uniqueness regression
   tests (**landed in this tree**: `regtest_network_from_upgrades` +
   spawn-time adoption in the zaino-state backends, de-mirrored testutils,
   unit tests beside the mapping, live regressions in
   `clientless/tests/validator_heights.rs`); remaining from this item: the
   config-type split (kind-only config type, heights only post-handshake)
   and the #1076 residue (lockbox fixtures, NU6.1 boundary test).
2. Infra: `NetworkKind` narrowing plus the strict form of the wallet-side
   change on `bump_to_NU6.3`. The standing directive (2026-07-06) is that
   the Validator is the *only* source of truth, enforced at compile time:
   the harness derives the wallet's `activation-heights.toml` from
   `Validator::get_activation_heights()`, and no client API accepts
   caller-supplied heights (the spec's original "lift the guard, accept
   caller heights" shape is superseded — a caller heights parameter is
   itself a second source).
3. Zaino pin bump: adapt to the narrowed `ZainodConfig`; the two
   `ironwood_activation` tests flip red (their expected
   `UnsupportedActivationHeights` panic disappears along with the error
   variant); replace `e2e::devtool::build_clients_at(port, &heights)` —
   which exists only to express the caller-supplied shape — with a
   derivation-based builder, so a fixture's heights are typed in exactly
   one place: the zebrad launch config. First live runs settle devtool
   note selection and the compact form of Orchard-spend data.
4. Deferred cells: testnet observational walk + post-flip Ironwood active
   cells (env-gated, evidence log); chain-walk cells for the cross-address
   restriction (Orchard pool-value monotonicity, same-receiver-change-only
   commitments); the Orchard spend-window/freeze sub-regime waits on a
   consensus source.

## Unrelated finding from the same session

The red `Integration tests / RPC tests shard-2/3` statuses on zaino commits
(zcash/integration-tests runs) are an upstream zallet break, not zaino's:
every run since 2026-07-03 ~23:25 UTC fails `zcashd_key_import{,_db}.py` in
zallet's `migrate-zcashd-wallet` with `UNIQUE constraint failed:
addresses.cached_transparent_receiver_address`, immediately after zallet
main's librustzcash/NU6.3 dependency bumps. No zallet issue existed for it
as of 2026-07-06.
