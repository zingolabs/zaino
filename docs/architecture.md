# Zaino architecture

How `zainod` turns a Zcash chain into the lightwalletd gRPC service, and why it is built this way.
Crate APIs live in each crate's `usage.md` (also its rustdoc); the per-method RPC table is
[rpc_api.md](./rpc_api.md); storage is the
[`zaino-persistence` guide](../packages/zaino-persistence/usage.md).

## 1. Who owns what

- **The validator** (zebrad, zakura) owns consensus: valid blocks, valid transactions, the mempool.
- **The wallet** owns keys.
- **Zaino** owns every answer *derived* from the chain.

Data a validator needs for consensus stays there and is not copied; everything derived is a Zaino
index and never falls back to a validator. So:

- Block, tree-state and transparent-address methods come from an index or not at all. A disabled
  index answers `UNIMPLEMENTED` naming it.
- `GetTransaction` goes to a validator: raw transaction bytes are consensus data it already stores
  ([why](../packages/zaino-grpc/usage.md#why-gettransaction-forwards)).
  `GetTaddressTransactions` works the same way: the index names the txids, a validator supplies the
  bytes.
- Zaino does not validate transactions. That would be a second mempool verifier (proofs,
  nullifiers, scripts, conflicts) tracking every network upgrade. Validators decide; Zaino checks
  only what the bytes, the next block and its own UTXO set decide (§5).

The principle behind the rest: **verify what Zaino can, trust validators for as little as
possible, take everything else from as many peers as possible.**

## 2. The map

```text
  trusted validators (1–N, configured)             peers ([p2p] on; default off)
  JSON-RPC or gRPC, + zebrad push streams          zebra-network peer set
                    │                                          │
                    └──────────────┬───────────────────────────┘
                                   ▼
                    zaino-traffic  TrafficBalancer      every request: lanes, hedge, blame
                                   │
       ┌───────────────────────────┼─────────────────────────────────────┐
       ▼                           ▼                                     ▼
  HeaderSync (chainview)      ObservationFold + PeerMempool        ChainView::submit
  → HeaderChain               → mempool view                       (SendTransaction)
       │ watch<VerifiedChain>       │
       ├──────────────────┐         │
       ▼                  ▼         │
  FinalizedFollower        Nfs (RAM)    │
  (zaino-sync)         tip overlay  │
       │ IndexerDataSink     │ Indexed
       ▼                     ▼      ▼
  5 index writers ──▶ zaino-snapshot Publisher ──▶ one Arc<GrpcSnapshot> per request
  (disk)  durable views                             → zaino-grpc, /status, /readyz, /metrics
```

Every block takes one of two paths, which never meet:

- **Final path** (durable): blocks at or below the final tip, folded and committed by each index
  writer. Writers never see a reorg.
- **Tip path** (RAM): blocks above the indexes' durable tips, folded in memory by the NFS,
  served at once, dropped once committed. Reorgs live here alone.

Both run the same `fold` per index, so a tip block is folded twice: once in RAM, once by its writer.

## 3. Sources and what each is trusted for

A **trusted validator** is one the operator configured (`[[trusted_validators]]`). A **peer** is
any node on the p2p network: discovered, unauthenticated, free to multiply. Nothing a peer says
counts until Zaino has checked it.

| Need                    | Trusted validators            | Peers                                  | Check                                     |
| ----------------------- | ----------------------------- | -------------------------------------- | ----------------------------------------- |
| Headers                 | ✓ (`getblockheader` runs)     | ✓ (`FindHeaders`), after the anchor    | trusted: link + work; peer: every rule    |
| Anchor (cold start)     | ✓ **only source**             | —                                      | —                                         |
| Blocks with a header    | ✓                             | ✓ (by hash)                            | hash + merkle root + coinbase height      |
| Blocks below the headers| ✓ **only source** (by height) | —                                      | coinbase height (body trusted)            |
| Mempool                 | ✓ (listing + fees)            | ✓ (quorum of listings)                 | txid recomputed; §5                       |
| Mined tx by id          | ✓ **only source**             | —                                      | txid recomputed                           |
| Finality                | ✓ (vouch)                     | ✓ (> 5 netgroups, no difficulty drop)  | §4                                        |
| Submission              | ✓ check (`checkadmission`)    | ✓ relays (zebra's, ~⅓ of peers)        | §6                                        |

**Why peers never decide the tip.** A peer count proves nothing an attacker cannot buy: anyone can
run ninety. Proof of work costs real mining however many peers present it. So peers are safe
sources of headers and of anything checkable against them, and weak sources of claims. Two claims
are taken from peers anyway, so Zaino keeps serving with every trusted validator down: finality and
mempool admission. Both count only outbound peers (Zaino chose them) spread over more than 5
netgroups (/16 IPv4, /32 IPv6; one operator ≈ one netgroup).

## 4. The best chain

The **Validated Header tree** (`zaino-header-chain`, memory only) holds every valid branch above the
final tip; the best tip is the leaf with the most cumulative work. `HeaderSync`
(`zaino-chainview`) is its only writer and publishes an immutable `VerifiedChain` on a watch that
the NFS, the final path and the `GrpcSnapshot` publisher read.

```text
  final tip                          best tip (most work)
  ───●──●──●──●──●──●──●──●──●──●──●  A  ◀─
                 └──●──●──●            B  kept: may still win; overtaking A = a reorg
```

- **Anchor.** No history from genesis. With no chain, or a trusted validator so far ahead its anchor
  lies above our fetch ceiling, the tree anchors at that validator's claim − max(depth, 113) (113 =
  the difficulty rule's context). Only a trusted validator can anchor.
- **Trusted headers** arrive in runs of up to 2,000 and enter on their parent link and nBits work
  alone: the validator already ran the rules.
- **Peer headers** (`[p2p]` on, once anchored, polled every 5 s) run every consensus rule in
  `zaino_header_chain::validate`: Equihash (200, 9) and hash ≤ target; nBits = zcashd's
  `CalculateNextWorkRequired` + ZIP 218; time after the median of 11, within 90 min of it, within
  2 h of our clock (a future header is deferred, not blamed). On regtest proof of work is off and
  nBits fixed at the limit, as zebrad does. A header failing a rule gets its sender blamed.
- **Bounds.** 32 side tips and 4 · depth side nodes; past either the lowest-work side leaf goes.
  The newest max(2 · depth, 113) final headers are kept. `depth` = `[sync] finalised_depth`,
  default and minimum 1,000 (lower only on regtest).

### Finality

A header proves work, not that its block's transactions are valid. A final block is never reorged
(the indexes hold it on disk), so finality is where Zaino stops being able to change its mind.

- **Vouched.** A trusted validator that had a block on its best chain vouches for it and every
  ancestor, for good (zebra commits only valid blocks). Vouches come from the last header of each
  trusted run, each validator's claimed tip, and its `getblockhash` answer at our best.
  **final = min(highest vouched, best − depth).** Peer headers never vouch.
- **On peers' word** (`[p2p]` on, nothing vouched that deep): best − depth still goes final when
  live outbound peers in > 5 netgroups last announced a block on our best chain at or above it,
  **and** no unfinal best-chain block has under half the work of the last vouched block. An
  attacker who eclipses us can feed a private chain, but its difficulty retargets to their
  hashrate, which shows up as that collapse. Both checks fail safe: a thin peer set or a real
  hashrate drop only delays finality.
- Neither holding: finality waits, the `finality_paused` alarm rises after 60 s, serving continues
  from RAM. A trusted validator's return vouches and finality resumes.

## 5. The mempool view

Every transaction the view knows is a **sighting**: its bytes (fetched once, txid recomputed), its
fee, a bitset of trusted validators listing it, a quorum flag and an `ours` flag.

**Servable** = listed by a trusted validator (not `Down` or catching up), admitted by a peer
quorum, or `ours` (accepted from our own submission). One trusted listing is enough: each trusted
validator is trusted.

**Peer quorum** (`[p2p]` on). Every 5 s each live outbound peer is asked for its whole mempool;
each listing counts for 15 s. A transaction is admitted when:

- more than 2/3 of the counted listings hold its exact id (txid + auth digest), from > 5 netgroups;
- its bytes decode, its expiry and (v5+) branch id fit the block above the verified best, it is not
  coinbase;
- every transparent prevout resolves in the served UTXO set or a held transaction's outputs, and
  the resulting fee is not negative.

It stays while more than 1/3 still list it. No fee floor (zebrad's and zakura's differ), no proofs,
signatures or scripts: each lister is a full node that ran those. A consensus refusal is remembered
10 min; an unknown prevout is retried next round. With honest peers two conflicting transactions
are never both admitted (one node's mempool holds at most one, and > 2/3 + > 1/3 must overlap);
a peer listing both can get both admitted and kept (`mempool_model.rs`). Cost to an attacker:
> 2/3 of our outbound peers across 6 netgroups, for a served transaction that never mines.

Transaction `inv` announcements are not used: nodes announce each transaction to a third of their
peers, once, so silence means nothing.

- **Leaving.** A transaction goes when nothing lists it and no quorum keeps it. A new tip clears
  nothing; an unlisted `ours` goes at the next move of the best block.
- **Gate.** `GetMempoolTx` / `GetMempoolStream` answer only while a `Live` trusted validator holds
  the verified best, or the polled peers span > 5 netgroups. Otherwise `UNAVAILABLE`, never an
  unverified mempool.
- **Fee**: a trusted listing's, or computed from the UTXO set for a quorum admission; none for an
  `ours` nothing has listed yet.

## 6. Submission

`SendTransaction` asks one trusted validator for a **verdict without a broadcast**, answers the
wallet from it, then **relays through the peers** and watches the transaction spread. Relaying
every wallet transaction through the same validators would tell any observer which transactions
came through Zaino, and let one validator censor every wallet behind it. The relay follows
Dandelion++'s originator with its fail-safe timer.

```text
precheck (nothing contacted): decodes (-22); expiry ≥ next block, v5+ branch = next block's (-25)
check: checkadmission on a random Live trusted validator holding the verified best
       refused → wallet gets that refusal at once, nothing relayed
       admissible → wallet answered Ok(txid) at once; transaction becomes `ours` (servable)
       unsupported / unanswered → next holder; none → unchecked (below)
attempt: peers — zebra's own relay, PushTransaction to ⌈ready/3⌉ random ready peers
         (no peers: a random untried trusted validator, sendrawtransaction)
unseen past propagation_threshold → next attempt, up to max_attempts
then, still unspread: one trusted sendrawtransaction (the fail-safe)
unchecked: Ok at the first acceptance (trusted Ok | any trusted listing | peer quorum);
           never accepted → that trusted push is the verdict; else UNAVAILABLE
```

- `checkadmission` (Zaino's zebra fork) runs `sendrawtransaction`'s checks, the fee policy and
  mempool conflicts included, without admitting or relaying: a refusal reaches the wallet in one
  call. It goes to a holder of the verified best only (a validator behind it refuses what spends a
  fresh output). A refusal is returned as gRPC `OK` carrying the validator's code and message.
- A validator without `checkadmission` (zcashd, stock zebrad) answers `-32601`: taken as its
  answer, never a failure (it stays `Live`), and the next holder is asked.
- A peer relay gets no reply and zebra names no recipient. **Spread** = a trusted validator never
  pushed to lists it, or a peer quorum admits it (> 2/3 of fresh listings: past the ~⅓ one relay
  reaches). The job ends at spread, when nothing outside its trusted entries can be watched, or
  when attempts run out (`unconfirmed`).
- Relays ride the peer set's existing connections: a zebrad peer drops a second connection from
  our IP, so a fresh connection per push would never deliver.
- `[p2p]` off: every attempt is a trusted `sendrawtransaction`.
- `[submission]`: `propagation_threshold_secs = 15`, `max_attempts = 4` (defaults).

## 7. Indexing

### Final path (`zaino-sync`)

`FinalizedFollower` streams `lowest durable tip + 1 ..= final tip` into one `IndexerDataSink<Block>`:

- Heights the verified chain has a header for: fetched by hash from any member (peers included),
  checked against the header (hash, merkle root, coinbase height).
- Below the header chain (bulk sync, a restart from an old index): by height, trusted only, batched
  (up to 8 per request), checked by coinbase height only. A trusted validator's body is trusted.
- Each block must link to every durable tip it extends and to the block sent before it; otherwise
  the follower stops and the process exits. Nothing is skipped.
- Queues are byte-bounded and a send reserves room in every queue first, so the slowest writer
  paces the fetch and memory stays bounded. A dropped queue or failed commit panics.
- Utxo-set resolves each transaction's fee and feeds compact-block through a `FeeSink`.
- A writer commits when its buffer fills, after a run ending at the final tip, and at shutdown.

### Tip path (`zaino-nfs`)

- **Root** = the lowest durable tip across indexes.
- The NFS folds only while the header just above the root is held and best is within 2 · depth of
  the root. Otherwise (first sync, an index enabled late) the final path does the work and the
  root is served.
- Each best-chain block above the root is fetched by hash, checked against its header, and folded
  over the indexes' durable views (utxo-set first: its fees feed compact-block).
- **Served tip** = the deepest folded best-chain node, else the root. A reorg moves it to the fork
  point at once, then forward as the new branch folds. Side branches stay until the header chain
  drops them, so switching back costs nothing.
- A node goes once every index has committed its height.

## 8. Serving: one `GrpcSnapshot`

`zaino-snapshot`'s publisher combines the NFS's latest `Indexed` and the chain view's latest
publish into one `Arc<GrpcSnapshot>`. Every gRPC request or stream, probe, scrape and state log loads
one and reads only it, so no answer mixes two moments.

- No height named → the served tip, which every enabled index holds. A height named → up to that
  index's own durable tip.
- `synced` opens when the served tip **is** the best block and no answering trusted validator
  claims a higher tip (`ChainViewSnapshot::claimed_past_best`: one already shown off our chain
  does not count, one not polled since best moved does), and stays while it is on the best chain
  within `depth` of it. At boot, header sync anchors `depth` below a validator's claim, so it
  stays shut until the header chain reaches that claim and the NFS has fetched the window.
- `/readyz` state (`GrpcSnapshot::standing`): `Serving`, `Syncing`, `Down` (every trusted validator
  down, or none holds the best block), or `Degraded` = that same cause while synced to a best block
  under 30 min old (peers carry the tip).
- `Unavailable` → gRPC `UNAVAILABLE`: no verified chain yet, no trusted validator holds the tip
  (mempool methods), or nothing served yet.
- `GetLightdInfo` never calls a validator: `blockHeight` = served, `estimatedHeight` = verified
  best, branch and next upgrade from the upgrade schedule (compiled in for mainnet and testnet,
  the validator's for regtest).

### `GetMempoolStream`

The protocol says "close the stream when a new block is mined"; Zaino makes that exact:

| ID | Rule |
| -- | ---- |
| S1 | Bound to the served tip's hash; ends `OK` once it moves (extend or reorg), never on an empty mempool |
| S2 | Never a transaction mined in the served tip's own non-final blocks |
| S3 | Opens with the servable mempool, then each new transaction once |
| S4 | Every message `height: 0` |

The stream ends only after the `GrpcSnapshot` already serves the new tip, so the `GetLatestBlock` a
wallet sends next sees it. One append-only log per served tip is shared by every subscriber, so
per-transaction work never scales with subscribers.

## 9. Talking to validators and peers

Every request goes through one `TrafficBalancer` (`zaino-traffic`); constants and the API are in
its usage guide.

- **Members.** Each trusted validator, in `priority` tiers (0 first); then the whole peer set as
  one member below every tier (zebra picks the peer). Peers answer headers and blocks only.
- **Lanes.** Per member, one permit is the poll's alone, so nothing delays tip following. Then
  `Control` (submit, headers), `Interactive` (tip blocks, tx lookups, mempool bytes) and `Bulk`
  (bulk blocks), each with one reserved permit; the rest is shared, control first.
- **Pick.** Best tier with room, then the preferred member, then the least loaded.
- **Failure.** A round asks each member once; slow answers are hedged to another member. Blocks
  retry every second forever; everything else ends unanswered after one round.
- **Blame.** The balancer cannot judge a value; the caller checks it and `report`s a bad one,
  which benches that member (60 s, doubling, ≤ 1 h). Absent is never blame. The peer set is never
  benched: zebra scores the lying peer instead.
- **Polling.** Each trusted validator is polled every 1 s (15 s while both zebrad push streams
  are up; a push event polls within 200 ms): `getblockchaininfo`, `getrawmempool true`,
  `getblockhash` at our best, and once a minute peers, version and end-of-service height. 10
  failures in a row → `Down`, probed every 30 s. A failing validator never stops the process.
- **Load on a zebrad** = `max_connections` (default 32, at least 5) + byte pacing
  (`max_mib_per_sec`). There is no request-rate limit.

## 10. When things fail

| Down                         | Chain + indexes                               | Mempool, `SendTransaction`             | Finality              |
| ---------------------------- | --------------------------------------------- | -------------------------------------- | --------------------- |
| one trusted validator        | ✓                                             | ✓                                      | ✓                     |
| every trusted, `[p2p]` on    | ✓ near the tip (peers); bulk sync below the headers stalls | ✓ quorum; `GetTransaction` fails | on peers' word, else paused |
| every trusted, `[p2p]` off   | tip stops                                     | `UNAVAILABLE`                          | paused                |
| every trusted, at cold start | nothing (only a trusted validator anchors)    | `UNAVAILABLE`                          | —                     |
| every peer                   | ✓ (trusted)                                   | ✓                                      | ✓                     |

## 11. Telemetry

Observation only: none of it changes a tip or what is served. State is read from one `GrpcSnapshot` at
scrape (`zaino_*` and `zaino_chainview_*` gauges, `/status`); events are emitted where they
happen. Alarms log once WARN when raised, once INFO when cleared:

| Alarm             | Raised when                                                                         |
| ----------------- | ----------------------------------------------------------------------------------- |
| stale             | a `Live` validator's tip trails its own clock-based height estimate by ≥ 24 blocks |
| end of service    | a validator's release halts within 8,064 blocks (a week) of its tip                 |
| partitioned       | two `Live` validators share no outbound peer                                        |
| eclipsed          | `Live` validators reach 1–2 distinct outbound peers in total                        |
| finality paused   | > depth unfinal blocks for 60 s with the final tip unmoved                          |

Per-transaction spread (`peers: x/y, trusted: x/y`) is tracked internally and feeds histograms
(first seen → first trusted listing → every trusted listing, residence).

## 12. Seams

A trait exists only where there must be more than one implementation:

| Trait                               | Implementations                                | Why                                      |
| ----------------------------------- | ---------------------------------------------- | ---------------------------------------- |
| `zaino_source::ChainDataSource`     | `ZcashLegacyRpcClient`, `ZebraIndexerGrpcClient`, `MockValidator` | RPC versions; tests control the data |
| `zaino_chainview::PeerSource` | zainod `ZebraPeers`, `MockPeers`             | real peers need sockets; Tor later       |
| `zaino_traffic::PeerTransport`      | zainod `ZebraPeers`, `MockPeerSet`             | keeps zebra-network out of the balancer  |
| `zaino_chainview::UnspentOutputs`   | zainod `ServedUtxos`, a `MockChain` `Ledger`   | the chain view sits below the indexes    |
| `zaino_persistence::PersistenceEngine`, `fs::Fs` | `DiskEngine`; `RealFs`, `SimFs`   | one storage contract; crash testing      |

Not seams: the gRPC service, each index, and the validator set (`TrafficBalancer` is not a
`ChainDataSource`, so a pool is never mistaken for one validator).
