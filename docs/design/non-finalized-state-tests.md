# Non-finalised state: test design

`live-tests/non-finalized-state/` holds the live suite for everything above
`tip − finalised_depth`: the pre-commit window, reorg detection and replay, the
serving gate, and every RPC whose answer depends on the tip. The machinery is
described in [precommit-state.md](./precommit-state.md). This document covers
what the suite asserts, and why it asserts it that way.

## Principles

1. **The validator is the oracle.** Every assertion about what zaino serves after
   a reorg compares it with zebrad's own answer over JSON-RPC: `getblock`,
   `z_gettreestate`, `getaddressbalance`, `getaddressutxos`, `getaddresstxids`,
   `getrawtransaction`, `getrawmempool`. A reorg test that only checks zaino
   against itself passes when both sides are wrong in the same way.
2. **The branches must differ in content, not just hash.** Branch A mines to
   `ORCHARD_MINER_ADDRESS`, which puts shielded commitments in the coinbase.
   Branch B mines to `FILLER_ADDRESS`, which puts none. A stale tree, a stale
   address row, or a stale compact block then produces a *different answer*, so
   an index that failed to roll back cannot match the oracle by accident. Each
   test also asserts that the two branches differ, so a harness change that
   quietly makes them identical fails loudly instead of making the test vacuous.
3. **Equivalence over enumeration.** These come from Zebra's `forked_equals_pushed`
   and Bitcoin Core's index-vs-fresh parity tests:
   - A reorged index equals the oracle.
   - A reorg to A → B → A restores A byte for byte.
   - A restarted index, which rebuilds pre-commit from scratch, equals the index
     that lived through the reorg.
4. **Deterministic injection.** `ValidatorBackend::reorg(depth, len, miner)` is
   `invalidateblock` on the first orphan, then waiting for the tip to retreat,
   then `generate`. It is the same recipe Zebra's own `force_zebra_reorg` uses.
   There is no second miner and no network partition. `reconsiderblock` brings a
   branch back, which gives the round-trip tests.
5. **A small window.** Each test sets `finalised_depth` explicitly, small enough
   that the durable/pre-commit seam and the window floor are both inside every
   fixture. The boundary tests then land exactly on `depth` and `depth + 1`.
6. **Hash-aware convergence.** Tests wait with `IndexerBackend::wait_for_tip`,
   which checks height *and* hash and rides out the UNAVAILABLE gate. Waiting on
   height alone passes on the losing branch.
7. **Down means frozen.** A test that needs zaino absent while the validator
   reorgs uses `ComponentPod::freeze` (SIGSTOP), never a bare kill: kubelet
   restarts a first crash at once, so a killed zainod can be back before the
   reorg lands. `thaw` resumes the same process (C18); `kill` after a freeze
   restarts it (C13).
8. **Every shape at unit level too.** `packages/zaino-sync/tests/reorg_model.rs`
   drives the real producer and a follower through random extensions, reorgs
   onto higher, equal and lower tips, bare retreats and jumps past the window,
   against a model of the best chain. It runs in seconds, so a shape is pinned
   there before it is pinned here.

## Contracts pinned

| #   | Contract                                                                                                                                                   | Where                    |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------ |
| C1  | After convergence, the served chain is the validator's best chain: every height, hash and `prev_hash` link                                                 | `reorg_shapes`           |
| C2  | An orphaned block hash answers NOT_FOUND. Only the canonical chain is served, as in lightwalletd; Bitcoin Core keeps stale blocks                            | `reorg_shapes`           |
| C3  | A height above a lowered tip is refused, never served from the orphaned branch                                                                             | `reorg_shapes`           |
| C4  | Blocks below the fork are byte-identical to what was served before the reorg                                                                               | `reorg_shapes`           |
| C5  | The tree state at every height equals `z_gettreestate`                                                                                                     | `reorg_shapes`           |
| C6  | Transparent balance, UTXOs and txids equal zebrad's address RPCs for both branches' miners                                                                 | `transparent_address`    |
| C7  | A → B → A (and repeated flip-flops) serve each branch byte-identically every time                                                                          | `round_trip`             |
| C8  | A concurrent reader sees one branch per response: ranges link, the only error is UNAVAILABLE, the served tip never dips below the fork parent, and a branch never comes back once replaced | `readers`                |
| C9  | Durable data is never rewritten: the finalised height is monotone and every durable hash is unchanged                                                      | `window`                 |
| C10 | A reorg deeper than `finalised_depth` halts zaino. It never serves after that point                                                                        | `window`, `restart`      |
| C11 | A tip retreat with no replacement block yet is followed, down to the durable boundary itself                                                              | `reorg_shapes::retreat*`, `transactions::remined_at_the_same_height` |
| C12 | `zaino_reorgs_total` is published at 0 from boot and increases by exactly 1 per rollback                                                                   | `reorg_shapes`, `round_trip` |
| C13 | A restart after a reorg serves identically. A reorg that happens while zaino is down is followed on restart                                                | `restart`                |
| C14 | `GetMempoolStream` ends when a reorg moves the tip                                                                                                         | `mempool`                |
| C15 | A transaction orphaned by a reorg leaves every index: its lookup, its nullifier, and address rows equal to zebrad's before the reorg, after it and after a re-mine. If rebroadcast, it re-mines at the same or a later height | `transactions`           |
| C16 | A reorg across the NU6.3 activation height serves the validator's blocks and trees on both sides of it                                                     | `activation`             |
| C17 | A burst of reorgs during catch-up, without waiting for convergence, still converges on the validator's chain                                               | `storm`                  |
| C18 | A tip first seen more than `finalised_depth` ahead, on another branch, is realigned onto before anything is finalised: no orphan goes durable, no crash | `stall`                  |

## Not reachable from this harness

- **A natural side chain.** Reaching one needs two miners and a partition, and
  ztest applies no network faults outside the sync tier. `invalidateblock`
  removes the branch from zebrad's chain set entirely, so `GetTransaction`'s
  "mined on a fork" sentinel (`0xffff…`) can't be produced here.
- **`GetSubtreeRoots` across a reorg.** A subtree needs 2¹⁶ commitments, which is
  out of reach for a regtest fixture.
- **Mempool re-admission of transactions from orphaned blocks.** zebrad drops
  them, and zaino mirrors the validator. C15 rebroadcasts explicitly, the way a
  wallet would.

## Wallets stay out of this suite

C15 needs a wallet only to *build* the shielded spend (zebrad has no wallet
RPCs). Nothing here asserts a wallet's own reorg handling: that tests the
wallet library, not zaino, and librustzcash's `sync::run` rewinds to
`at_height − 10` with no floor, so a reorg at height ≤ 10 fails its rewind
outright. Wallet-through-zaino behaviour belongs in `e2e`.
