# The verified chain: headers in, verified blocks out

The end state of how Zaino follows the chain: headers from every source verified into one
most-work chain, every block checked against it, one sender (`zaino-nfs`) feeding the indexes
through the final stream, and the p2p network as a first-class source. It replaces trust-routing (fetch only
from the validators that hold the tip) with verification (fetch from anyone, check what arrives).

Related: the mempool, submission and telemetry ([chainview.md](./chainview.md)), what the sink
promises the indexes ([data-sink.md](./data-sink.md), [nfs.md](./nfs.md)),
who is trusted for what ([boundaries.md](./boundaries.md)).

Status: **design, decisions taken 2026-10-06**; phases in §11.

## 1. What is wrong today

Measured against the code, not the intent (2026-10-06):

| Gap                                                                                                                                                                 | Effect                                                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Bulk blocks are never checked against the verified header chain (`hash_at` has no caller in sync)                                                                   | a validator's stale or forked branch is delivered **final** before anything proves it leads to the verified tip; caught later, fatally (`BelowWindow`) |
| A block's transactions are never checked against its header (merkle check is a `TODO`)                                                                              | a correct header with altered transactions passes                                                                                                      |
| Fetches are restricted to the tip's holders, and bulk caches them                                                                                                   | if every cached holder is down, bulk retries forever while others hold the tip                                                                         |
| Finality lives twice (`Publisher.final_tip`, `ChainHead.highest`)                                                                                                   | two rules that must agree; the window can hold 2·depth+1 blocks                                                                                        |
| Holding is computed by walking each validator's window of links, plus `vouched`, which never expires                                                                | ~600 lines of walk machinery with its own races; a validator that reorged away can stay a holder                                                       |
| `HeaderChain` sits behind a `std::Mutex` held across a 2,000-header verify, finalized on the runtime; a store error only logs and stops header sync                 | blocked runtime threads; a silent stall where the failure policy says crash                                                                            |
| The branch tree is unbounded; first sync takes any valid chain                                                                                                      | cheap testnet headers grow memory; an eclipsed first sync follows a low-work chain                                                                     |
| Header rules are constants: NU7 (Testnet 4,465,026) changes the spacing and the averaging window; `nVersion` is read unsigned; a non-minimal solution length passes | every Testnet header past NU7 is refused; two consensus mismatches                                                                                     |
| Peers serve no headers or blocks; zaino-peers advertises a pre-NU7 protocol version                                                                                 | the p2p half of the design is unbuilt; v7 zebras will drop us after NU7                                                                                |
| Headers are parsed and hashed twice; test doubles serve invented hashes                                                                                             | wasted work; nothing below the live suite exercises verification                                                                                       |

## 2. Decisions

| #   | Decision                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **No new zebra patches.** Attribution and everything sync-critical is done at Zaino's own boundary. The zebra fork is only rebased onto upstream's primary branch (for NU7 and protocol 170,180).                                                                                                                                                                                                                                                                   |
| 2   | **Minimum chain work gates peer-only bests, never finality.** Finality needs a trusted holder (peers alone never finalize), so a work floor adds nothing there; dropped from finality 2026-10-06 (an unbounded in-memory first sync was its only effect). A per-network floor (zcashd's `nMinimumChainWork`) returns with peers (phase 5) as `credible`: below it a best no trusted validator holds is not followed or served. Not a checkpoint: it trusts no hash. |
| 3   | **Follow the verified best before any trusted validator holds it.** Blocks are verified, so the NFS follows proof of work; only finality waits for a holder.                                                                                                                                                                                                                                                                                                   |
| 4   | **`[p2p]` on by default.**                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| 5   | **Block commitments are checked incrementally**, off the hot path, as their own follower (§8).                                                                                                                                                                                                                                                                                                                                                                      |

## 3. The shape

```text
  trusted validators (RPC)                 peers (p2p, zaino-peers)
     │ poll: tip, holds?(h), listing          │ inv (tx, block) ◀── zebra-network PeerSet (crawl, listen)
     │ headers by height, blocks by hash      │ headers, blocks, tx bytes ◀── WorkPool (isolated, attributed)
     ▼                                        ▼
  ┌──────────────────────────────────────────────────────────┐
  │ HeaderSync  (one task; owns HeaderChain; no lock)         │
  │   stage A, parallel:  decode · version · Equihash ·       │
  │                       hash ≤ target · run linkage         │
  │   stage B, in order:  attach · nBits · median time ·      │
  │                       work · best · prune                 │
  │   finality:           boundary final ⇔ held by a trusted  │
  │                       validator                           │
  └───────────────────────────┬──────────────────────────────┘
                              │ watch<Arc<VerifiedChain>>
          ┌───────────────────┼───────────────────────┬─────────────────┐
          ▼                   ▼                       ▼                 ▼
      ChainView           Nfs (zaino-nfs)          zebra ChainTip    CommitmentAuditor
      holders, mempool,   fetch (h, hash) from     (start height,    (history tree,
      spread, submission  anyone → check → fold    min peer version)  off the hot path)
                              │
                              ▼
                          final stream ─▶ index writers; snapshots ─▶ serving
```

**`VerifiedChain`** is the one value everything downstream reads, published on a `watch` whenever
the best chain or the final boundary moves. Cloning it is a refcount; a reader holds one for a
whole step, so nothing it reads can change under it.

```rust
pub struct VerifiedChain {
    final_tip: BlockRef,            // durable in the header store; never moves back
    above: imbl::Vector<BlockHash>, // final_tip.height + 1 ..= best, the best branch
    finals: HeaderView,             // the header store at final_tip (an immutable DiskView)
    work: Work,                     // the best tip's cumulative work
}
impl VerifiedChain {
    pub fn best(&self) -> BlockRef;
    pub fn final_tip(&self) -> BlockRef;
    pub fn hash_at(&self, height: Height) -> Option<BlockHash>;      // any height ≤ best
    pub fn header_at(&self, height: Height) -> Option<HeaderFields>; // merkle root, time, bits
    pub fn locator(&self) -> Vec<BlockHash>;                          // for getheaders
    // phase 5: credible() = work ≥ the network floor (gates a peer-only best, decision 2)
}
```

It lives in `zaino-header-chain`; `zaino-sync` depends on that crate for it and on nothing in
chainview.

**Vocabulary** (one meaning each, everywhere): **best** = the verified most-work tip;
**final tip** = the last final block; **holder** = a trusted validator whose best chain contains a
block; **claim** = a validator's own reported tip; **source** = anything that answers requests
(validator or peer); **announcer** = a peer that sent an `inv`; **entry** = where a submission is
pushed.

## 4. Headers

### Rules

| Rule          | Check                                                                                                                                                                                                 |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Version       | `nVersion` read as `int32`: high bit set = invalid; ≥ 4 (> 4 stays valid)                                                                                                                             |
| Solution      | compactSize minimally encoded; 1,344 bytes (36 on regtest)                                                                                                                                            |
| Proof of work | Equihash (200, 9) valid; SHA-256d(header) ≤ `ToTarget(nBits)` (off on regtest)                                                                                                                        |
| Difficulty    | `nBits` = ThresholdBits(h): averaging window and spacing **by height** (17 / 75 s; NU7: 102 / 25 s), median of 11, damping 4, clamps; Testnet minimum difficulty (gap > 6×spacing, NU7: > 18×spacing) |
| Time          | > median of the previous 11; ≤ MTP + 90 min (Mainnet h ≥ 2, Testnet h ≥ 653,606); ≤ clock + 2 h (non-consensus: **deferred, never cached invalid**)                                                   |
| Linkage       | `prev_hash` = the parent's recomputed hash; genesis = the network's                                                                                                                                   |
| Work          | Σ `2^256 / (target + 1)`; ties: first received (as zebra 6.4+)                                                                                                                                        |

`CONTEXT` = largest window + 11 (113 under NU7). The difficulty function takes `(mean target, timespan, height)` so zcashd's `pow_tests.cpp` vectors test it directly.

### HeaderSync

One task owns the `HeaderChain`. Inputs arrive on one channel; nothing else touches the chain.

- **Stage A (stateless, parallel on the blocking pool):** decode, version, solution encoding,
  Equihash, hash ≤ target, and linkage *within* the run. A run that fails is cut at the failure;
  the source is blamed (§6).
- **Stage B (contextual, in order, on the owner):** attach to a known parent (tree node or final
  tip), `nBits`, median time, max time, work, best, prune. Cheap: no hashing.
- **Requests.** Validators: `getblockheader <h> false` batches (2,000) — the bulk path for a first
  sync. Peers: `getheaders(locator, stop)` (160 a message) over the WorkPool — the tip path,
  triggered by a block `inv` or a timer. A run whose first header has an unknown parent is an
  orphan: refused, and the next request uses the locator, which reaches back to the fork.
- **Bounds.** A node that does not descend from the final tip is gone (it can never become best).
  Above it, at most `4·depth` nodes and at most 32 side-branch tips; past either bound the
  lowest-work leaf is evicted. The best branch is never evicted.
- **Finality.** The boundary (`best − depth`) becomes final only when some trusted validator holds
  that block (§7); work never gates it (decision 2). Held during a first sync, the tree stays at
  `depth` plus one fetch batch. When no trusted validator holds it, finality pauses and the
  `finality_paused` alarm rises. A header-store commit error ends the process.
- **Output.** A new `VerifiedChain` whenever best or final moves, and nothing else.

### Locator

zcashd's: the best tip, ten consecutive ancestors, then doubling steps, ending at the final tip
(and genesis for a first sync). zcashd announces a new tip only as an `inv` of its hash
(there is no `sendheaders`), answered with `getheaders(locator, stop = that hash)`; a syncing
zcashd ignores `getheaders`, so silence is not a failure.

## 5. Blocks

**Every block is checked, so any source may serve it.**

```text
  want (height, hash) from the VerifiedChain
        │
        ▼
  route: cheapest source holding it: WorkPool peers (near the tip) · trusted validators (bulk)
        │ getblock <hash> 0  /  getdata(MSG_BLOCK)
        ▼
  check: header hash = hash · coinbase height = height · merkle root(recomputed txids) = header's
        │ mismatch → that source misanswered (§6): next source
        ▼
  deliver
```

- A body that fails its check is a bad answer, **never** an invalid header (ZIP 256: a v5 block's
  hash does not commit to its authorizing data, so a valid header can travel with a mutated
  body). The header stays; the block is asked of someone else.
- The merkle root covers every txid and a v5 txid commits to the effecting data, so every field
  Zaino derives is covered. Authorizing data is checked by §8, off the hot path.
- Zebra serves only its **best** chain by hash (RPC and p2p): a block on our best branch that a
  validator holds only as a side chain is not served by it. Routing tries every source before
  waiting.

## 6. Peers, without patching zebra

zebra-network's `PeerSet` routes by load and does not report who answered, and a request it
routes as a "find" is stall-tracked against a chain tip. Zaino keeps the `PeerSet` for what it is
good at and asks peers its questions over connections it owns.

| Part                                        | What it does                                                                                                                                                                                                                        |
| ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `PeerSet` (zebra `init`)                    | crawling, the address book, inbound `inv` (tx and block, attributed by zebra), transaction broadcast                                                                                                                                |
| **`WorkPool`** (Zaino)                      | a small set (default 8) of `connect_isolated_tcp_direct` connections to sampled peers, one netgroup each; every request (`getheaders`, `getdata` blocks and transactions) goes over one of them, so every answer has a known sender |
| `ChainTip` (Zaino implements zebra's trait) | the best height and hash from the `VerifiedChain`: an honest start height in our `version`, and the correct minimum peer version after an upgrade                                                                                   |

- **Attribution.** An answer on a WorkPool connection came from that connection's peer.
  Misbehaviour is scored locally (zcashd's scores: invalid PoW or solution 100, high hash 50,
  orphan 10, time rules 0) and at 100 the peer is dropped from the pool, kept out for 24 h, and
  reported on zebra's misbehaviour channel.
- **Pool upkeep.** Refill to target from the address book (live, current protocol, unused
  netgroup, not banned); rotate the oldest connection every 15 minutes so no fixed set of peers
  can surround us; replace a connection that times out twice in a row.
- **No stall drops.** Requests never pass through the `PeerSet`'s stall tracker.
- **Protocol version.** Comes from the zebra release (170,180 Testnet / 170,190 Mainnet after NU7).
- **Status and metrics:** WorkPool members (address, age, requests, failures, score), announcers,
  bans; headers and blocks answered per source.

## 7. Trusted validators: holding is a question, not a walk

A trusted validator **holds** block `(h, hash)` iff its best chain has `hash` at `h`:
`getblockhash h` = `hash`. One batched poll answers everything Zaino asks of a validator:

```text
  poll = [ getblockchaininfo, getrawmempool true,
           getblockhash <best.height>, getblockhash <boundary.height> ]
```

- **Holders of the best tip** = validators answering `best.hash` at `best.height`.
- **Finality** reads holders of the boundary block, and nothing else (no work floor: decision 2).
- **Agreement** (per validator, for status and alarms) from its claim `(ch, chash)` and the two
  answers: `Agreed` (claim = best), `Ahead` (holds best, claims higher), `Behind`
  (`hash_at(ch) = chash`, below best), `Diverged` (none of these; zebra #11133: a validator can sit
  on a losing branch indefinitely, so this is an alarm, never an error).
- A race (the validator moved between items of one batch) yields one wrong poll; every poll
  re-asks, and finality only ever needs one true answer at one moment.
- **Downward closed.** An answer on the verified chain at `h` holds every verified block ≤ `h` (a
  hash commits to its ancestry), so a validator's standing is one height, its *reach*: the claim
  counts as an answer, and so does the last header of each run header sync reads off its best
  chain (`getblockheader` by height = the same question). That run answer is what lets bulk sync
  finalize batch by batch between polls.
- Each poll replaces the last one's answers; a failed poll forgets them (no answer = holds
  nothing). An item failing alone (above its tip: zebrad `-32602`) is no answer at that height.

This replaces `EndpointChain`, `Walk`, link batches, mid-walk races and `vouched`.

## 8. Block commitments, incrementally (decision 5)

A `CommitmentAuditor` follows the tree-state index's published roots and the `VerifiedChain`,
off the hot path:

- before Heartwood: `hashFinalSaplingRoot` = the index's Sapling root after the block;
- Heartwood/Canopy: the ZIP 221 history root; NU5 on: `BLAKE2b("ZcashBlockCommit", historyRoot ‖ authDataRoot ‖ 0^32)`;
- the history tree is appended one leaf per block (O(log n) hashes, `zcash_history`), and resets
  at each upgrade; `authDataRoot` is the auth digests the parser already computes;
- a mismatch halts finality and alarms (it means the tree-state fold or the parser disagrees with
  consensus). It never blocks serving or the NFS.

## 9. Following the chain: the NFS

Built as `zaino-nfs` ([nfs.md](./nfs.md) §6; the producer this section first described is
deleted). The NFS follows the `VerifiedChain` and nothing else.

- **One finality**: the header chain's final tip. The NFS keeps no tip of its own.
- **No walk-back**: the verified chain names every hash; a reorg's fork is a comparison.
- **Impossible states are asserts**: a fork at or below the final tip cannot come from the header
  chain. A fetched block that fails its check is the source's fault, retried elsewhere.
- **Restart**: each index's durable tip must be `c.hash_at(height)`; nothing is sent while the
  final tip is below it; a mismatch is the one fatal case (`NfsError::Diverged`).
- **One pipeline**: `concurrency` blocks fetched or folding ahead of the next one needed, for
  every height.
- **Reorgs stay in the NFS**: the final stream carries final blocks only
  ([data-sink.md](./data-sink.md)); a reorg moves the served snapshot to the fork point and on
  along the new branch.
- **Serving compares hashes**: a snapshot's tip is the deepest folded block on the verified best.

## 10. Engineering and tests

This is the most failure-prone code in Zaino: many sources, adversarial inputs, time, reorgs and
restarts. It is built so that every behaviour is a pure function that a property test can drive,
and every invariant is written down, asserted, and seen to fire.

### Structure: pure cores, thin drivers

Every component is two halves:

| Core (synchronous, deterministic, no I/O)                                           | Driver (async: channels, timers, requests)                    |
| ----------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `HeaderChain`: insert run, finalize, prune, best, locator                           | `HeaderSync`: requests, stage A on the blocking pool, publish |
| `Holders`: poll answers + `VerifiedChain` → holders, agreement, finality permission | the validator poller                                          |
| `NfsCore`: `VerifiedChain` + bodies + folds + durable tips → sends, folds, fetches  | `Nfs`: fetch, check, fold, send, publish                      |
| `WorkPoolCore`: members, scores, bans, refill and rotation choices                  | WorkPool: connect, request, time out                          |
| mempool fold, `Overheard`, submission `Job` (exist today)                           | `ChainView`, `PeerWatch`, `Submission`                        |

A core's API is `fn step(&mut self, input) -> Vec<Output>`-shaped: no clock (time is an input),
no randomness (a seeded RNG is an input), no I/O. Every core has `fn check(&self)` that asserts
all its invariants; it runs after every step in tests and debug builds.

Each core method starts with its preconditions as `assert!`s naming the invariant, e.g.
`assert!(run.first().prev == parent.hash, "header run attaches to its parent")`. A violated
precondition is a bug in the caller, so it panics; bad *input* (a lying source) is never a panic,
it is an `Err` that names the source.

### Invariants

**Header chain**

|     | Invariant                                                                                            |
| --- | ---------------------------------------------------------------------------------------------------- |
| H1  | best = the max-work leaf of the tree; on equal work, the first received                              |
| H2  | every node descends from the final tip; the final tip never moves back; a final header never changes |
| H3  | only valid headers enter: each mutation of a valid header is refused by its own rule                 |
| H4  | the tree stays within its bounds; pruning never removes a node on the best branch                    |
| H5  | `hash_at` = the best path; a published `VerifiedChain` never changes                                 |
| H6  | the boundary becomes final only when held by a trusted validator (and is never held back by work)    |
| H7  | a header from the future is deferred, then accepted once the clock passes it                         |
| H8  | an invalid header is blamed on its sender and never enters the tree                                  |

**NFS and the final stream** (built as `NfsCore` N1–N6, [nfs.md](./nfs.md) §9)

|     | Invariant                                                                                                     |
| --- | ------------------------------------------------------------------------------------------------------------- |
| P1  | every sent or folded block is `c.hash_at(h)` for the chain it was taken under, with a matching merkle root    |
| P2  | the final stream: every height once, ascending, final, never retracted                                        |
| P3  | nothing at or below the final tip is ever replaced                                                            |
| P4  | at quiescence the served tip = best, every index's durable block = the final tip (within one batch)          |
| P5  | a lying source never causes a wrong delivery; a silent one never stalls a block another source has                                                                              |
| P6  | on restart every durable tip is checked against the header chain; a diverged index halts                                                                                        |

**Chain view and validators**

|     | Invariant                                                                                                                                                         |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| V1  | holders(block) = the validators whose best chain contains it (oracle)                                                                                             |
| V2  | agreement = the oracle's classification                                                                                                                           |
| V3  | every served mempool transaction is listed by a live trusted validator or is ours; the stream sends each once per epoch; the epoch moves iff the best block moves |
| V4  | spread counts = the oracle's; first-seen never moves later                                                                                                        |
| V5  | a submission answers exactly once; peers first, one netgroup each; the verdict only from a trusted validator                                                      |

**Peers**

|     | Invariant                                                                             |
| --- | ------------------------------------------------------------------------------------- |
| N1  | the WorkPool never holds a banned peer, holds one per netgroup, and refills to target |
| N2  | every scored lie is scored against the peer that told it                              |
| N3  | a block `inv` triggers at most one header request per hash in flight                  |

### Test layers

1. **Real chains below the live suite.** One builder (`zaino_primitives::testing::Chain`, phase 1)
   makes regtest blocks with real header bytes, merkle roots and linkage, branching anywhere, and
   — under a test `Params` whose difficulty rule accepts any `nBits` — varying work, so most work
   ≠ highest. `MockChain` serves it.
1. **Core models (proptest, against naive oracles).** One per core: the header chain against a
   naive tree (H1–H8), `Holders` against a set computation (V1, V2), `NfsCore` against a
   fold-from-genesis oracle (P1–P3), `WorkPoolCore` against a naive scoreboard
   (N1, N2), plus the existing mempool, `Overheard` and `Job` models. Swarm-style generation:
   whole input kinds switched off per case.
1. **The network simulation.** All drivers wired as zainod wires them, over a simulated world, on
   a paused single-threaded runtime with a seeded RNG, so a failing case replays exactly and
   shrinks:
   - **World:** a block DAG from the builder; trusted validators, each with its own best chain,
     answering with zebra's semantics (best chain only by hash, `getblockhash`, errors as zebra
     words them); peers that are honest, slow, silent, or lying (invalid PoW, wrong `nBits`,
     unlinked runs, mutated bodies, a low-work eclipse chain, floods of side branches).
   - **Events:** mine on any branch, relay, reorg at any depth above final, partition a validator,
     stall a validator on a fork, take a source down, restart zainod (drop non-durable state),
     announce, submit, list or evict a transaction, advance the clock.
   - **Checked after every event:** H, P, V and N invariants that hold at all times; **at
     quiescence:** liveness (P4, the final tip reaches `best − depth` once held, every honest
     block reachable is delivered).
1. **Fire drills.** For every invariant check, a planted bug that makes it fire (a check never
   seen firing is not known to work).
1. **Real data.** Mainnet header fixtures (every `nBits` reproduced), contiguous Testnet captures
   across 299,188 (minimum difficulty), 584,000 (Blossom) and 4,465,026 (NU7); zcashd's
   `pow_tests` vectors; the equihash crate's (200, 9) vectors; zebra's boundary blocks for the
   merkle check.
1. **Heavy runs.** Like persistence: `PROPTEST_CASES=1000` loops of the core models and the
   simulation for at least three minutes after any change to these crates, and an hour-long soak
   before a release. The command goes in `CLAUDE.md` next to the persistence one.
1. **Live suite** (ztest): the integration and test plan (separate document) — multi-validator
   partitions, real peers, reorgs staged with `invalidateblock` + `generate`, wallets.

## 11. Phases

0. **NU7 on Testnet (urgent).** Rebase the zebra fork onto upstream's primary branch (protocol
   170,180, NU7 branch id `0x77190AD9`); header rules by height; `nVersion` signed; minimal
   solution length; Testnet fixtures.
1. **Test foundations**: the real-header chain builder, `MockChain` on it, `FakeNode` gone, headers
   decoded once.
1. **Header chain**: single owner, stages A/B, bounds, locator, `VerifiedChain`, finality alarm,
   store error fatal; the header-chain model. (Minimum work moved to phase 5: decision 2.)
1. **Holders by question** (§7) and the chain view on the `VerifiedChain`: `Holders` core and
   model; walk machinery removed.
1. **Fetch on the verified chain**: checked fetch from any source, one finality; done as
   `zaino-nfs` ([nfs.md](./nfs.md)), which replaced the producer and its gates.
1. **Peers**: `WorkPool` (core, model, driver), `ChainTip`, block `inv`, headers, blocks and
   mempool bytes from peers, status and metrics, `[p2p]` on by default; minimum chain work
   (`credible`) gating a best no trusted validator holds.
1. **The network simulation**, fire drills, heavy runs.
1. **Commitment auditor** (§8).
1. **Live suite and soak** against the integration and test plan.
