# non-finalized-state

Live tests for everything above `tip − finalised_depth`: the non-finalized state, reorg
handling, the served snapshot tip, and every RPC whose answer depends on the tip. Each test runs
a regtest zebrad and a zainod built from this tree on the ztest Kubernetes harness.

## How the tests are built

**The validator is the oracle.** After a reorg, Zaino's answers are compared with zebrad's own
JSON-RPC (`getblockhash`, `z_gettreestate`, `getaddressbalance`, `getaddressutxos`,
`getaddresstxids`, `getrawtransaction`, `getrawmempool`, `getblockchaininfo`). A test that only
compared Zaino with itself would still pass if both sides were wrong in the same way.

**The branches differ in content.** Branch A is mined with an Orchard coinbase
(`mine_to(Pool::Orchard)`), so every block appends shielded commitments. Branch B is mined to
`FILLER_ADDRESS` and appends none. An index that failed to roll back therefore cannot match the
oracle by accident. Tests that compare trees also assert the branches really differ, so a harness
change that made them identical fails instead of turning the test vacuous.

**Equivalence over enumeration.** A reorged index equals the oracle; returning from B to A
serves A byte for byte; a restarted index equals the one that lived through the reorg.

**Deterministic reorgs.** `validator.reorg(depth, len, miner)` calls `invalidateblock` on the
first orphan, waits for the tip to retreat, then mines `len` replacement blocks (`len = 0` is a
bare retreat). `reconsiderblock` brings a branch back. No second miner, no partition.

**A small window.** Each test sets `finalised_depth` itself, small enough that the durable /
non-finalized seam sits inside the fixture, so boundary cases land exactly on `depth` and
`depth + 1`.

**Wait on height and hash.** `indexer.wait_for_tip` matches both and polls through UNAVAILABLE.
Waiting on height alone passes while Zaino still serves the losing branch.

**Down means frozen.** To keep Zaino away while the validator reorgs, tests `freeze` the pod
(SIGSTOP), never just kill it: kubelet restarts a first crash immediately, so a killed zainod can
be back before the reorg lands. `thaw` resumes the same process; `kill` after `freeze` restarts it.

Shapes are pinned first at unit level in `packages/zaino-nfs` (`src/core/model.rs`,
`src/tests.rs`), which run in seconds. This suite proves them against a real validator.

## Contracts

| # | Contract | Test |
| --- | --- | --- |
| C1 | Served chain = validator's best chain: every height, hash and `prev_hash` link | `reorg_shapes::every_index_serves_the_validators_chain_after_a_reorg` |
| C2 | Orphaned block hash → NOT_FOUND (only the canonical chain is served) | same |
| C3 | Height above a lowered tip is refused, never served from the orphaned branch | same |
| C4 | Blocks below the fork byte-identical to before the reorg | same |
| C5 | Tree state at every height from the fork parent up = `z_gettreestate` | same |
| C6 | Transparent balance, UTXOs and txids = zebrad's address RPCs, for both branches' miners | `transparent_address::address_index_moves_orphaned_coinbase_to_the_winning_miner` |
| C7 | Flip-flopping A ↔ B repeatedly serves each branch byte-identically every time | `round_trip::flip_flopping_between_two_branches_serves_each_byte_identically` |
| C8 | Concurrent reader: one branch per response, ranges link, only error = UNAVAILABLE, tip never below the fork parent, a replaced branch never returns | `readers::readers_see_one_branch_at_a_time_through_a_reorg` |
| C9 | Durable data never rewritten: finalized height monotone, durable blocks unchanged | `window::deepest_legal_reorg_never_rewrites_durable_data` |
| C10 | Reorg deeper than `finalised_depth` halts Zaino; it never serves again | `window::reorg_below_the_window_halts_instead_of_serving`, `restart::reorg_below_durable_while_down_halts_on_restart` |
| C11 | Tip retreat with no replacement block is followed, down to the durable boundary | `reorg_shapes` cases `retreat`, `retreat_onto_the_durable_tip`; `transactions` case `remined_at_the_same_height` |
| C12 | `zaino_reorgs_total` = 0 from boot, +1 per rollback | `reorg_shapes`, `round_trip`, `stall` |
| C13 | Restart after a reorg serves identically; a reorg while Zaino is down is followed on restart | `restart::restart_after_a_reorg_serves_identically`, `restart::reorg_while_down_is_followed_on_restart` |
| C14 | `GetMempoolStream` ends when a reorg moves the tip, including at the same height | `mempool::mempool_stream_ends_when_a_reorg_moves_the_tip` |
| C15 | Tx mined only on an orphan leaves every index (lookup, nullifier, address rows = zebrad's); rebroadcast, it returns on the first block after B | `transactions::orphaned_transaction_leaves_every_index_and_returns_where_remined` |
| C16 | Reorg across NU6.3 activation serves the validator's blocks, trees and branch id on both sides | `activation::reorg_across_nu6_3_activation_matches_the_validator` |
| C17 | Burst of reorgs during catch-up, never awaited, still converges on the validator's chain | `storm::reorg_storm_during_catch_up_converges_on_the_validators_chain` |
| C18 | Tip first seen > `finalised_depth` ahead on another branch: realign before finalizing, no orphan goes durable, no crash | `stall::stalled_through_a_reorg_past_the_window_realigns_without_finalizing_orphans` |

## Out of reach here

- `invalidateblock` drops the losing branch from zebrad entirely, so zebrad never holds a side
  chain to compare against. A natural side chain would need two miners and a partition.
- `GetSubtreeRoots` across a reorg needs 2¹⁶ commitments, too many for a regtest fixture.
- zebrad never re-admits transactions from orphaned blocks, and Zaino mirrors its mempool. C15
  rebroadcasts explicitly, the way a wallet would.

## Wallets

C15 uses a wallet only to build the shielded spend (zebrad has no wallet RPCs). A wallet's own
reorg handling tests the wallet library, not Zaino; wallet-through-Zaino behaviour belongs in
`live-tests/e2e`.
