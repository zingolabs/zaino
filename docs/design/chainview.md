# Chainview: the best chain and the mempool, from many sources

Chainview is how Zaino knows what the chain is and what is waiting to get into it. It answers
three questions for the rest of the system:

- **Which chain is best?** Block sync follows it; every index is built from it.
- **What is in the mempool?** `GetMempoolTx` and `GetMempoolStream` serve it.
- **How far has a transaction spread?** Every transaction carries `peers: x/y, trusted: x/y`.

The principle behind every rule below: **verify what Zaino can, trust validators for as little
as possible, and take everything else from as many peers as possible.** Zaino verifies proof of
work on block headers itself. It does not validate transactions, so it leans on a small set of
validators it trusts for exactly that, and nothing more.

## Status

| Part                                                                                           | State                                  |
| ---------------------------------------------------------------------------------------------- | -------------------------------------- |
| Mempool view, telemetry (§5, §11)                                                              | built                                  |
| `GetMempoolStream` on a write-once log, keyed by the served tip (§5; `zaino-snapshot`)         | built                                  |
| `GetLightdInfo` from one global snapshot (§12)                                                 | built                                  |
| `[[trusted_validators]]`, any-trusted admission, non-fatal validator failure (§5, §7, §10)     | built                                  |
| Trusted-validator links: lanes, budgets, batched mempool bytes (§7)                            | built                                  |
| `GetMempoolTx` projection rendered once per transaction (§5)                                   | built                                  |
| `trusted: x/y` + spread timeline per transaction, tracked and on `/statusz` (§5, §11)          | built (`peers: x/y` with `[p2p]` on)   |
| Validator release + end-of-service height, alarm a week ahead (§11)                            | built                                  |
| Batched poll tick (holding asked by `getblockhash`), routing for mined txs (§7, §9)           | built                                  |
| Block fetch: the NFS's checked, least-loaded fetch over every validator (`zaino-nfs` `fetch.rs`) | built                                |
| `peers/trusted` on the extension service (§5)                                                  | planned                                |
| Push streams (§7)                                                                              | built                                  |
| Header chain: proof of work, most-work tip, holders, finality gate (§2–§4)                     | built; headers from trusted validators |
| Submission: random entry, watched, resubmitted (§6)                                            | built, peer + trusted entries          |
| `zaino-peers`: zebra-network, attributed `inv`, isolated push (§8)                             | built                                  |
| Peers in the view: mempool sightings, submission entries (§5, §6), via zainod `[p2p]`         | built                                  |
| Peers as a header and block source (§2, §8)                                                    | planned                                |

## 1. Sources

```text
     peers (discovered, many)                    trusted validators (configured, 1–2)
     open p2p protocol                           JSON-RPC (+ zebrad's push streams)
        │                                           │
        │ headers · blocks · mempool ids/bytes      │ headers · blocks · mempool listing + fees
        │ broadcast                                 │ mined tx by id · broadcast
        ▼                                           ▼
   ┌──────────────┐                          ┌──────────────────┐
   │   PeerSet    │                          │   RpcClient ×N   │  lanes, budgets, batches
   └──────┬───────┘                          └────────┬─────────┘
          └──────────────────┬────────────────────────┘
                             ▼
              verify everything that can be verified
                             │
          ┌──────────────────┴───────────────────┐
          ▼                                      ▼
   HeaderChain (best chain)               MempoolView (sightings)
```

A **peer** is any node on the Zcash p2p network. Peers are discovered, unauthenticated and cheap
to create, so nothing a peer says counts until Zaino has checked it. A **trusted validator** is a
zebrad the operator configured: ours, or a partner's with their consent and credentials.

What each source is used for, and what makes it safe:

| Need                    | Peers          | Trusted           | Check                                                      |
| ----------------------- | -------------- | ----------------- | ---------------------------------------------------------- |
| Headers / best tip      | ✓ preferred    | ✓                 | proof of work, difficulty, time, linkage (§2)              |
| Blocks                  | ✓ near the tip | ✓ bulk throughput | hash + merkle and auth-data roots against the header chain |
| Mempool bytes           | ✓ first        | on miss           | txid recomputed from the bytes                             |
| Mempool admission + fee | —              | ✓ **only source** | zebrad admitted it after full validation                   |
| Mined transaction by id | —              | ✓ **only source** | peers serve mempool transactions only                      |
| Finality confirmation   | —              | ✓ **only source** | §4                                                         |
| Submission (§6)         | ✓ random entry | verdict, last     | success = a trusted validator lists it                     |
| Propagation             | ✓ count        | ✓ count           | telemetry, never a vote                                    |

The trusted validators' responsibility is the right-hand column's "only source" rows, and the
design keeps shrinking it: anything a peer can supply verifiably comes from peers.

**Why peers cannot vote.** A peer is an IP address that completed a handshake. Anyone can run
nine of them, or ninety, and the reachable mainnet network is small (21 nodes on the current
protocol in a 2026-10-06 crawl). A count of peers proves nothing an attacker cannot buy. Proof of
work is different: a heavier chain costs real mining however many peers present it. So peers are
safe sources of headers and of anything checkable against them, and unsafe sources of claims.

**Why Zaino does not validate transactions.** It would need proof verification, a nullifier-set
index, an outpoint index, script verification and mempool conflict tracking: a second
implementation of zebrad's mempool verifier, kept in lockstep with every network upgrade. Proofs
alone are not enough (a re-spent note carries valid proofs and fails only against the nullifier
set). A trusted validator answers the question; Zaino asks it.

## 2. The best chain: proof of work

The best chain is the valid header chain with the most cumulative work. Headers come from every
source; how one was received never changes how it is checked.

| Rule          | Check                                                                                                                                                        |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Proof of work | Equihash (n = 200, k = 9) solution valid, and the header hash ≤ its target                                                                                   |
| Difficulty    | `nBits` = what the adjustment rule expects: 17-block averaging window, median time of 11, damping 4, clamps +16 % / −32 %, testnet's minimum-difficulty rule |
| Time          | later than the median of the previous 11; at most 2 h ahead of the local clock                                                                               |
| Linkage       | `prev_hash` = the parent's hash, recomputed from its bytes                                                                                                   |
| Work          | `2^256 / (target + 1)` per header, summed from genesis                                                                                                       |

Equihash costs 156 µs per header on one core (200 mainnet headers, 2026-10-06). One header every
75 s at the tip is free, and verifying all ~3.5 M mainnet headers takes about nine minutes on one
core, a fraction of that across cores, since each solution is independent. The difficulty rule is
a port of zebra-state's `AdjustedDifficulty` (zebra-state itself brings RocksDB); its test oracle
is the real chain, every mainnet header's `nBits` reproduced.

On regtest zebrad disables proof of work, and so does Zaino: a property of the declared network,
like the reorg bound.

Headers are verified **from genesis, once**. The verified chain is persisted (§3), and that record
is the checkpoint every restart resumes from: never a height a validator or a compiled-in list
supplied.

Above the final boundary the chain is a tree: every valid branch is kept, and the best tip is the
leaf with the most work.

```text
  final boundary (tip − 1000)
        │
  ──────●──●──●──●──●──●──●──●──●──●──●  A   work 1000.7  ◀── best tip
                          │
                          └──●──●──●      B   work 1000.2  (kept: may still win)

  B gains two blocks → work 1000.9 → best tip moves to B: a reorg at the fork
```

## 3. Where it sits in sync

Sync is headers-first: the header chain decides the chain, and block sync fills it in.

```text
  peers ─┐
         ├─ headers ─▶ HeaderChain ── best tip (watch) ───────────▶ Nfs ──▶ final stream ──▶ indexes
trusted ─┘              │    │                                       ▲  │
                        │    └─ hash_at(h) ── is this block on it? ──┘  │
                        │                                               │
                        └─ header store (final records, own files)      │
                                                                        │
  peers ─┐                                                              │
         ├─ blocks by hash (any source, each checked) ──────────────────┘
trusted ─┘
```

`HeaderChain` lives in its own crate, `zaino-header-chain`: the verification rules as pure
functions, the branch tree in memory, and a store on `zaino-persistence`. It publishes the best
tip on a `watch` channel and answers `hash_at(height)` on the best chain.

The **NFS** (`zaino-nfs`) follows the header chain's `VerifiedChain` alone. Any source may serve
any block: the NFS checks each fetched block's hash against `hash_at(height)` and its merkle root
against `header_at(height)`, refuses one that differs, and asks another source. One finality (the
header chain's final tip); a reorg is a hash comparison against the nodes it holds above it; the
indexes see final blocks only ([pipeline.md](./pipeline.md)).

The **header store** follows the same two watermarks as every index ([nfs.md](./nfs.md)): the
tree above the final boundary is memory, and a header is written once it is final. One fixed-size record per height (hash, time,
`nBits`, cumulative work: ~80 B, ~280 MB for mainnet), encoded by named functions next to a golden
test, as every disk layout is.

**Existing indexes are checked by their tip alone.** Every block hash commits to its parent's, so
an index whose durable tip hash equals the verified chain's hash at that height holds exactly the
verified chain below it. The NFS compares each index's durable tip against the
chain it follows; under the header chain that comparison is against verified work.

## 4. Finality

```text
  height ─▶     ... ─────────────────────────┬──────────────────────────────────┐
                    final (on disk)          │       non-final (memory)          │ best tip
                                             │◀────────── 1000 blocks ──────────▶│
                                     final boundary
                                             ▲
                     a block crosses only if a trusted validator holds it
```

A header proves work, not that its block's transactions are valid. Someone who spends a whole
block's mining on an invalid block can make it the best tip briefly; honest validators reject it,
the network outmines it, and Zaino reorgs away. To keep such a block out of the durable indexes,
**a block is written as final only once a trusted validator holds it** on its own chain.

This costs no latency: the boundary is 1,000 blocks behind the tip, and a trusted validator has
held a block for an hour or more by then. With every trusted validator down that long,
finalization pauses and alarms; serving continues from the non-final window.

## 5. The mempool view

Every transaction the view knows is a **sighting**:

```text
  Sighting
  ├─ raw        bytes, fetched once (peers first; txid recomputed)
  ├─ fee        a trusted validator's listing (it resolved the prevouts)
  ├─ trusted    bitset over configured trusted validators    → trusted: 1/2
  ├─ peers      set of connected peers that announced it     → peers: 7/9
  └─ ours       broadcast by this Zaino
```

- **Servable** = listed by **any** trusted validator, or `ours`. Each trusted validator is trusted,
  so one admission proves a transaction valid, and serving it then gets an incoming payment to a
  wallet as early as possible. A wallet's own send is servable the moment we relay it.
- **Peer-only sightings** (no trusted listing yet) are held: their bytes are ready the moment a
  trusted validator lists them, and they are streamed, labeled unverified, through a Zaino
  extension service for wallets that opt in. `GetMempoolTx` and `GetMempoolStream` never serve
  them: a peer's listing is a free claim, and a forged shielded "pending payment" needs no valid
  proof if nobody checks one.
- **`peers: x/y, trusted: x/y`** is tracked for every sighting and exposed through the same
  extension service; lightwalletd's `RawTransaction` has no field for it, so the protocol stays
  unchanged. `y` counts only the sources whose mempool is being read right now (a catching-up or
  down validator lists nothing, so it is not in `y`). Each sighting also keeps its timeline: first
  seen, first trusted listing, and the first moment every reading trusted validator listed it (`ChainViewSnapshot::spread`).
- A transaction leaves the view when no source lists it anymore. An unmined transaction survives a
  block that did not include it; an `ours` nobody lists is dropped at the next tip move.
- With no trusted validator live, `GetMempoolTx` / `GetMempoolStream` refuse with `UNAVAILABLE`
  rather than serving an unverified mempool. The extension's unverified stream keeps going.

### `GetMempoolStream`

```text
  subscribe ──▶ every tx currently in the mempool ──▶ each new tx, once, as it arrives ──▶ closes on a new block
                                                                                              │
                wallet resubscribes ◀─────────────────────────────────────────────────────────┘
```

The protocol defines it this way ("a stream of current Mempool transactions … close the returned
stream when a new block is mined", `service.proto`), and lightwalletd does the same. Sending the
current transactions first is what lets a wallet that resubscribes after each block see a
transaction that arrived while it was reconnecting. The stream closes when the **served** tip
moves (after the NFS folds the new block, [global-snapshot.md](global-snapshot.md) §4), never
when the mempool empties: an empty mempool with no new block is a live, silent stream. A stream's
end therefore means the next `GetLatestBlock` already serves the new tip.

**Written once, read by cursors.** Each served tip gets one append-only log: the mempool when it
opened, then every transaction that becomes servable (the view's `arrivals` between two of its
publishes), each encoded once and shared by refcount. A subscriber is a pointer to that log and a
cursor into it, so per-transaction work never scales with subscribers and per-subscriber state
never grows with arrivals. The log is the global snapshot's (`zaino-snapshot/src/feed.rs`): the
view keeps no feed and never learns the served tip.

Measured (2026-10-06, one core for the server, real HTTP/2 over loopback, 2,000–5,000
subscribers, 2 KB transactions): each arrival costs the server ~2–3 µs per subscriber (1.9 µs in
bursts, which coalesce into one write per connection), essentially h2 framing and the socket
write. The zaino part is ~0.1 µs. Opening costs are bound by bytes: the protocol resends the whole
mempool to every subscriber on every block. A resumable extension stream (subscribe from a
cursor, never closed by a block) is what removes that, and is the planned answer.

## 6. Submission: one random entry node, watched until it spreads

Zaino submits each transaction to **one** node sampled at random from the network, watches the
mempools it can see for it, and resubmits through a fresh random node when it has not spread
within `propagation_threshold`. This is part of Zaino's protocol, not a transport detail.

```text
  SendTransaction
        │ precheck (local, instant): parses · expiry above the tip · branch id = next block's
        ▼
  attempt n ── sample entry ∉ {entries 1..n-1}, netgroup unused ── isolated push ──▶ entry node
        │                                                                              │ verifies,
        │   watch: trusted listings (push stream / poll) · peers' inv announcements    │ gossips
        ▼                                                                              ▼
  listed by a trusted validator ──────────────────────────────▶ accepted: wallet answered, `ours`
  announced by a peer ≠ entry, not yet listed by a trusted one ──▶ spreading: keep waiting
  neither within propagation_threshold ──▶ attempt n + 1 (until max_attempts)
  max_attempts exhausted ──▶ verdict submit to one random trusted validator (sendrawtransaction)
```

**Why.** Today every wallet transaction first appears on the network from the same one or two
validators, so any observer of the network knows which transactions came through Zaino, and a
validator that drops or delays them silently censors every wallet behind it. A random entry per
transaction spreads first appearance across the network. A watched resubmission routes around a
dead, slow or censoring entry with no operator in the loop. It also takes the trusted validators
off the submission path, which is the point of this design (§1: shrink what they are trusted for).
The rule is Dandelion++'s originator with its fail-safe timer (Fanti et al., 2018): one stem hop
chosen by the sender, and a timer that resubmits when the transaction does not diffuse.

**Mechanics.**

- **Entry candidates:** the address book (§8), not only connected peers: recently live, on the
  current network upgrade's protocol version, a different netgroup (/16, /32 for IPv6) from every
  earlier attempt's entry. The sample space is the address book, so it grows with crawling and
  costs no standing connections.
- **The push:** a fresh `zebra_network::connect_isolated_tcp_direct` connection per attempt,
  `PushTransaction`, closed. zebra-network's `PeerSet` cannot target a peer (it routes by P2C,
  inventory or broadcast), and the isolated connection carries no address book and no node state
  that would link attempts to each other or to Zaino's standing peers. Over Tor later, when
  zebra-network re-enables `connect_isolated_tor` (zebra #5492).
- **Watching:** a trusted validator listing it is the success signal (it verified it). A peer
  other than the entry announcing it means spreading: the attempt is not failed, though only a
  trusted listing answers the wallet. The entry's own announcement counts for nothing (a
  black-holing entry can echo it to us alone).
- **No verdict from peers:** a peer answers a push with nothing. A rejection reason exists only as
  a trusted validator's `sendrawtransaction` error, so it is the last step, after the attempts. The
  local precheck catches the common wallet errors (expired, built for the wrong upgrade) at once,
  without it.
- **Lifecycle:** each submission is a tracked job in the view (an `ours` sighting with its
  attempts, entries and timestamps), so `trusted: x/y`, `peers: x/y` and the attempt count are
  reported per transaction. A job ends when the transaction is mined or its expiry height passes.

**What the wallet sees.** `SendTransaction` answers when the job reaches a verdict: success at the
first trusted listing (seconds: the entry verifies, then gossips at once; detection is immediate
on a push stream), the precheck's or the verdict submit's rejection, or `UNAVAILABLE` when neither
a listing nor a verdict came. This is slower than a direct `sendrawtransaction` by one propagation
hop, and it is the honest answer: success means the network has it.

**Configuration.**

```toml
[submission]
propagation_threshold_secs = 15   # per attempt; measured on mainnet before it is tuned
max_attempts = 4                  # then the verdict submit
```

**With `[p2p]` off (or before the peer network is up):** the candidates are the trusted validators, each attempt an RPC
`sendrawtransaction` to one of them (a verdict per attempt), and the others' listings are the
watch. That is the same protocol over a sample space of 1–2.

**Not done here:** keeping a transaction alive after acceptance (resubmitting one every mempool
evicted before it was mined) is the same job extended to expiry; it waits on a decision about how
long Zaino owns a wallet's transaction.

## 7. Talking to a trusted validator

Every request Zaino sends a trusted validator goes through one traffic balancer
([traffic-balancer.md](./traffic-balancer.md), `zaino-traffic`): the chain view's polls,
header runs, mempool bytes and submissions, the NFS's blocks and gRPC's lookups. It owns the
connection budget (request classes with reserved and ceiling-capped permits, so a bulk-sync burst
or a wallet storm never delays a poll), the request rate, every cadence, retry, hedge and bench;
each validator's `zaino_source::RpcClient` makes one attempt per call. A zebrad JSON-RPC server
admits 100 connections in total: keep `zaino nodes × max_connections` (default 32, at least 6)
under it.

- **Bytes are charged per body chunk as it is read** (the `governor` crate): an exhausted budget
  stops reading, and TCP backpressure slows the validator's send. Requests are charged per call
  (the balancer's), so a batch of N costs N.
- **Batches.** JSON-RPC batch requests (zebrad hands an array straight to jsonrpsee, whose batch
  limit is unlimited) carry N calls in one round trip and one permit; a work-queue-full item is
  that item's own refusal, re-asked by the balancer. A poll is at most two round trips: `getblockchaininfo` +
  `getrawmempool true` + `getblockhash` at the final boundary and the best (who holds them:
  [verified-chain.md §7](./verified-chain.md#7-trusted-validators-holding-is-a-question-not-a-walk))
  (+ `getpeerinfo`, `getinfo`, `getdeprecationinfo` once a minute), then one
  `getrawtransaction` batch for new mempool
  transactions, bounded at 100 calls and 8 MiB so the hex reply stays under zebrad's 50 MiB
  `max_response_body_size`. zebrad answers `getblockchaininfo` even on an empty state (genesis,
  mempool inactive), so there is no separate readiness probe.
- **Push streams.** zebrad's `Indexer` gRPC streams `ChainTipChange` and `MempoolChange`. When a
  validator's config names an `indexer_address` and both streams are up, each event wakes its
  poll (`TrafficBalancer::pushed`; coalesced: 200 ms between polls) and the reconcile interval
  is 15 s; absent or broken, it polls every second. The listing stays the one source of truth (an
  event only decides when to read it), which is safe because zebrad ends a lagged stream
  (`while let Ok(..) = recv()` exits, then `UNAVAILABLE`) rather than skip events, and the stream
  coming up or going down polls at once.
- A failing trusted validator degrades and re-probes on a capped backoff ladder; it never ends
  the process, and boot does not wait for it.

## 8. Talking to peers

The p2p layer is `zebra-network`: handshake, address book, crawler, per-peer limits, and a peer
set exposed as a load-balanced tower service. Zaino embeds it with an inbound service that answers
nothing, and binds its listener to loopback: Zaino serves no peer. It brings `zebra-chain`, whose
crypto crates match the forks Zaino already patches in, and whose difficulty and work types the
header chain reuses.

| Request                                      | Used for                                        |
| -------------------------------------------- | ----------------------------------------------- |
| `FindHeaders`                                | headers past our best tip, from many peers (§2) |
| `BlocksByHash`                               | blocks near the tip and on failover             |
| `MempoolTransactionIds`, `TransactionsById`  | mempool sightings and bytes (§5)                |
| `AdvertiseTransactionIds`, `PushTransaction` | broadcast (§6)                                  |

Not bulk sync: zebrad answers one block per request with one request in flight per peer, so p2p
block fetch is latency-bound; bulk throughput comes from trusted validators, or from bootstrapping the indexes off a published
index archive (`[snapshot]` in zainod's config).

Etiquette: mainnet peers keep one connection per IPv4 address, and the network is small. The peer
target stays modest, and a host that also runs a zebrad competes with it for the same slot on
every peer.

## 9. Routing

One balancer routes every request: [traffic-balancer.md](./traffic-balancer.md) §3 (members,
classes, tiers, hedge / retry / blame) replaces this section's earlier table. Within the best
tier with room, the cheaper of two picked at random, load = peak-EWMA latency × requests in
flight (power-of-two-choices, as in Finagle, linkerd and zebra-network's own peer set); trusted
`priority` tiers first, then peers. Submission's entry choice stays §6's (`entries()` →
`submit(member, raw)`).

## 10. When things fail

| Down                    | Chain + index-backed methods            | Default mempool | Unverified stream | Finality       |
| ----------------------- | --------------------------------------- | --------------- | ----------------- | -------------- |
| one trusted validator   | ✓                                       | ✓               | ✓                 | ✓              |
| every trusted validator | ✓ (headers + blocks from peers, slower) | `UNAVAILABLE`   | ✓                 | pauses, alarms |
| every peer              | ✓ (headers + blocks from trusted)       | ✓               | —                 | ✓              |
| everything              | tip stops; stale-tip alarm              | `UNAVAILABLE`   | —                 | pauses         |

## 11. Telemetry

Observation only: none of it changes a tip, a sighting's servability, or what is served. The view
raises each condition per fold (`Alarms`); the global snapshot's publisher logs it once when it
rises and once when it clears, and its raw inputs are `zaino.chainview.*` gauges set from the
snapshot at scrape ([global-snapshot.md](global-snapshot.md) §5). The view itself emits only
events (spread histograms, submission outcomes).

- **Stale tip:** the best tip's time trails the clock by ≥ 24 blocks' worth (the chance of a
  natural 30-minute gap is ≈ e^-24): stalled, or eclipsed.

- **Trusted validator diverged:** its chain does not hold the best tip and is not behind it. The
  node is broken, or the network is feeding a chain the validators reject.

- **Finality paused:** no trusted validator holds the block at the final boundary.

- **Thin network:** few distinct peers, or trusted validators sharing no outbound peer.

- **End of service:** a trusted validator's release halts within a week of its tip
  (`getdeprecationinfo`, zebrad ≥ 6.3; mainnet only).

Per trusted validator: state, agreement with the best tip (`Agreed`, `Ahead`, `Behind`,
`Diverged`), latency, failures (state, latency and failures: the traffic balancer's `MemberTable`,
joined at report time), release (build, user agent, protocol) and end-of-service height, all on
`/statusz` (`zaino_snapshot::Report`), with every alarm including `finality_paused`. Per transaction: `peers: x/y, trusted: x/y` as it moves, with histograms of
first-seen → first trusted listing, first → every trusted listing, and residence until a block
(or eviction) removes it.

## 12. `GetLightdInfo`

The most frequent wallet call (50× any other on the mainnet fleet) never waits on a validator. It
renders from one global snapshot load (`Snapshot::lightd`): `blockHeight` = its served tip, and
the network estimate, upgrade schedule and branch = the last `getblockchaininfo` of a trusted
validator holding the best block, read by its poll. One load, so height and branch never come
from two moments. No holder, or nothing served yet: `UNAVAILABLE`.

## Configuration

```toml
[[trusted_validators]]
jsonrpc_address = "golden-mainnet-zebra.vaquita-altair.ts.net:8232"
indexer_address = "golden-mainnet-zebra.vaquita-altair.ts.net:8230"  # push streams; absent = poll

[[trusted_validators]]
jsonrpc_address = "eu-zebra.example:8232"

[p2p]
enabled = true
peer_target = 16
```

`[[trusted_validators]]` replaces `[source]` and `[[chainview_peers]]`, with no compatibility
shim; routing replaces `fetch.primary_validator`.

## Phases

1. `GetLightdInfo` from the view (§12): **done**
1. Per-validator `RpcClient` lanes, `[[trusted_validators]]`, non-fatal failure, any-trusted admission,
   `peers/trusted` counts and the extension service (§5, §7, §10): **done** but the extension
   service
1. Batched ticks, routing (§7, §9): **done**
1. Push streams (§7): **done**
1. Header chain: verification from genesis, header store, most-work tip driving sync, block
   checks, finality gate (§2–§4): **done**; submission's trusted-only form (§6): **done**
1. Peers: headers, blocks, mempool sightings, submission entries, the unverified stream (§5, §6,
   §8): the `zaino-peers` crate, mempool sightings and submission entries **done**; headers and
   blocks from peers next

Phase 5 takes headers from trusted validators' RPC. The source carries no trust (every header is
verified the same way); it lets the verifier be proven against mainnet before the p2p transport
adds failure modes of its own.

## Measurements behind this design

Mainnet fleet, 2026-10-06, Prometheus, 24 h: four Hetzner nodes, one validator on tekau over the
tailnet.

| Signal                                    | Value                                                 |
| ----------------------------------------- | ----------------------------------------------------- |
| `GetLightdInfo` mean / p99 before §12     | 146–354 ms / 1.0–4.9 s (`GetLatestBlock`: 0.5 ms p50) |
| trivial validator call mean / p99         | 157–364 ms / 0.9–2.6 s                                |
| `getblock` p99, timeouts                  | 20–25 s, 86–221 per node per day                      |
| `GetTransaction` p50 / p99                | 0.57 s / 8 s                                          |
| golden's outbound peers exposing JSON-RPC | 4 of 69                                               |
| Equihash verification                     | 156 µs per header, one core                           |

The cost was the path to one distant validator, not its work; and with one validator, the fleet's
chain was that validator.
