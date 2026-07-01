# Ironwood support targets the Zebra backend only

Ironwood (the NU6.3 shielded pool, carried by v6 transactions) is implemented only
in the Zebra-backed `zaino-state` path. We deliberately do **not** port the
`valargroup/lightwalletd` `adam/lightwalletd-nu7-v6-parser` byte parser into the
zcashd-backed `zaino-fetch` path, even though `zaino-fetch` has the analogous
hand-rolled transaction parser.

Rationale: zcashd reaches end-of-support on 2026-07-15, before Ironwood mainnet
activation on 2026-07-21, so the network is Zebra-only before any v6 transaction
can exist — a v6 transaction will never legitimately reach the `zaino-fetch`
parser. zaino is already deprecating zcashd (see ADR-0001, and ADR-0005 which makes
`zcashd_support` opt-in / default-off). `zaino-fetch` keeps its existing
behaviour of rejecting transaction version ≥ 6 with a clean error.

Consequence: Zebra (`zebra_chain::Transaction`) is the source of v6 parsing;
zaino's own Ironwood work is proto changes, compact-action extraction, and tree
state — not transaction byte parsing.

## Tracking: NU6.3 consensus parameters

The NU6.3 consensus parameters were finalized upstream in
`zcash/librustzcash` PR #2509 ("Crate releases supporting NU6.3 testnet
activation"):

- **Consensus branch ID: `0x37a5_165b`** (replaces the earlier placeholder
  `0xffff_ffff`).
- **V6 transaction version group ID: `0xD884B698`** (replaces placeholder
  `0xffff_ffff`).
- Testnet activation height: `4_134_000`; mainnet activation: not yet set.

Dependency-pin note: as of `ZcashFoundation/zebra@nu63-ironwood` rev
`c662473b9`, zebra still carried the placeholder branch ID `0xffff_ffff`. The
zebra rev zaino pins for the coding PR must be one that has adopted the real
`0x37a5_165b`; otherwise zaino would disagree with the finalized librustzcash
value and reject valid v6 transactions.
