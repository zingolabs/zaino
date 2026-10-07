# Non-finalized state: test design

`live-tests/non-finalized-state/` is the live suite for everything above `tip − finalised_depth`:
the non-finalized state, reorg detection, the served snapshot tip, and every RPC whose answer
depends on the tip. [nfs.md](./nfs.md) describes the machinery.
This document covers what the suite asserts, and why it asserts it that way.

## Principles

**The validator is the oracle.** Every assertion about what Zaino serves after a reorg compares it
with zebrad's own JSON-RPC answer: `getblock`, `z_gettreestate`, `getaddressbalance`,
`getaddressutxos`, `getaddresstxids`, `getrawtransaction` and `getrawmempool`. A reorg test that
only checks Zaino against itself still passes when both sides are wrong in the same way.

**The branches differ in content, not just in hash.** Branch A is mined by the validator's
configured miner, `mine_to(Pool::Orchard)`, which puts shielded commitments in every coinbase.
Branch B is mined to `FILLER_ADDRESS`, which puts none. A stale tree, address row or compact block
then gives a different answer, so an index that failed to roll back cannot match the oracle by
accident. Each test also asserts that the branches differ, so a harness change that quietly made
them identical fails loudly instead of making the test vacuous.

**Equivalence over enumeration.** Following Zebra's `forked_equals_pushed` and Bitcoin Core's
index-versus-fresh parity tests, we assert equivalences. A reorged index equals the oracle. A reorg from A to B and back to A restores A byte for byte. A
restarted index, which rebuilds its non-finalized state from scratch, equals the index that lived
through the reorg.

**Deterministic injection.** `ValidatorBackend::reorg(depth, len, miner)` calls `invalidateblock`
on the first orphan, waits for the tip to retreat, then mines `len` replacement blocks. This is the
recipe Zebra's own `force_zebra_reorg` uses, with no second miner and no network partition.
`reconsiderblock` brings a branch back, which is what the round-trip tests use.

**A small window.** Each test sets `finalised_depth` explicitly, small enough that both the seam
between durable and non-finalized data and the window floor sit inside the fixture. The boundary
tests then land exactly on `depth` and `depth + 1`.

**Hash-aware convergence.** Tests wait with `IndexerBackend::wait_for_tip`, which checks height and
hash and keeps polling through UNAVAILABLE (nothing served yet at boot). Every request answers at
its snapshot's tip, which trails the best while the NFS catches up, so waiting on height alone
passes on the losing branch. `zaino_index_synced` = the served tip is the verified best (it stays
on within `finalised_depth` behind, off once it leaves the best chain).

**Down means frozen.** A test that needs Zaino absent while the validator reorgs uses
`ComponentPod::freeze` (SIGSTOP), never a bare kill, because kubelet restarts a first crash at once
and a killed zainod can be back before the reorg lands. `thaw` then resumes the same process (C18),
and `kill` after a freeze restarts it (C13).

**Every shape at unit level too.** `packages/zaino-nfs` pins the shapes first: the `NfsCore` model
(`core/model.rs`) drives random verified-chain evolutions (extensions, reorgs at random depth,
same-height replacements, retreats, finality, restarts) with lying and silent sources, checking
every published snapshot against folding each index from genesis along the best chain; the driver
test (`tests.rs`) does the same with all five real folds, and zainod's pipeline test follows a
reorg end to end over gRPC. They run in seconds, so we pin a shape there before we pin it here.

## Contracts pinned

| #   | Contract                                                                                                                                                                                                                    | Where                                                                |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| C1  | After convergence, the served chain is the validator's best chain: every height, hash and `prev_hash` link                                                                                                                  | `reorg_shapes`                                                       |
| C2  | An orphaned block hash answers NOT_FOUND. Only the canonical chain is served, as in lightwalletd, whereas Bitcoin Core keeps stale blocks                                                                                   | `reorg_shapes`                                                       |
| C3  | A height above a lowered tip is refused, never served from the orphaned branch                                                                                                                                              | `reorg_shapes`                                                       |
| C4  | Blocks below the fork are byte-identical to what was served before the reorg                                                                                                                                                | `reorg_shapes`                                                       |
| C5  | The tree state at every height equals `z_gettreestate`                                                                                                                                                                      | `reorg_shapes`                                                       |
| C6  | Transparent balance, UTXOs and txids equal zebrad's address RPCs for both branches' miners                                                                                                                                  | `transparent_address`                                                |
| C7  | Flip-flopping between A and B, repeatedly, serves each branch byte-identically every time                                                                                                                                   | `round_trip`                                                         |
| C8  | A concurrent reader sees one branch per response: ranges link, the only error is UNAVAILABLE, the served tip never dips below the fork parent, and a branch never comes back once replaced                                  | `readers`                                                            |
| C9  | Durable data is never rewritten: the finalised height is monotone and every durable hash is unchanged                                                                                                                       | `window`                                                             |
| C10 | A reorg deeper than `finalised_depth` halts Zaino, which never serves after that point                                                                                                                                      | `window`, `restart`                                                  |
| C11 | A tip retreat with no replacement block yet is followed, down to the durable boundary itself                                                                                                                                | `reorg_shapes::retreat*`, `transactions::remined_at_the_same_height` |
| C12 | `zaino_reorgs_total` is published at 0 from boot and increases by exactly 1 per rollback (a published tip leaving the best chain)                                                                                           | `reorg_shapes`, `round_trip`                                         |
| C13 | A restart after a reorg serves identically, and a reorg that happens while Zaino is down is followed on restart                                                                                                             | `restart`                                                            |
| C14 | `GetMempoolStream` ends when a reorg moves the tip                                                                                                                                                                          | `mempool`                                                            |
| C15 | A transaction orphaned by a reorg leaves every index: its lookup, its nullifier, and address rows equal to zebrad's before the reorg, after it and after a re-mine. If rebroadcast, it re-mines at the same height or later | `transactions`                                                       |
| C16 | A reorg across the NU6.3 activation height serves the validator's blocks and trees on both sides of it                                                                                                                      | `activation`                                                         |
| C17 | A burst of reorgs during catch-up, fired without waiting for convergence, still converges on the validator's chain                                                                                                          | `storm`                                                              |
| C18 | A tip first seen more than `finalised_depth` ahead, on another branch, is realigned onto before anything is finalised, so no orphan goes durable and nothing crashes                                                        | `stall`                                                              |

## Not reachable from this harness

- A natural side chain needs two miners and a partition, and ztest applies no network faults
  outside the sync tier. `invalidateblock` removes the branch from zebrad's chain set entirely, so
  `GetTransaction`'s "mined on a fork" sentinel (`0xffff…`) cannot be produced here.
- `GetSubtreeRoots` across a reorg would need 2¹⁶ commitments, which is out of reach for a regtest
  fixture.
- Transactions from orphaned blocks are never re-admitted to the mempool, because zebrad drops them
  and Zaino mirrors the validator. C15 rebroadcasts explicitly, the way a wallet would.

## Wallets stay out of this suite

C15 needs a wallet only to build the shielded spend, since zebrad has no wallet RPCs. Nothing here
asserts a wallet's own reorg handling, because that tests the wallet library rather than Zaino. It
would also fail on its own terms: librustzcash's `sync::run` rewinds to `at_height − 10` with no
floor, so a reorg at height 10 or below breaks its rewind outright. Wallet-through-Zaino behaviour
belongs in `live-tests/e2e`.
