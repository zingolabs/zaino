# TrafficBalancer: one way Zaino talks to validators and peers

Status: **phase 2 built** (2026-10-07), **simplified** (2026-10-08): every trusted-validator
request goes through `zaino-traffic` (§9 steps 1–7 and 9); peers as members (step 8) wait for the
WorkPool. Builds on [chainview.md](chainview.md) §7–§9, [pipeline.md](pipeline.md). Boundary
with `global-snapshot.md` in §6.

## 1. Before: five schedulers over the same validators

| Caller | Asks | Pick | Concurrency | Retry / back-off | Blame |
| --------------------------------------- | ----------------------------------------------------------- | ----------------------------- | ----------------------------------- | -------------------------------------------------------- | ---------------------------------------- |
| `zaino-nfs` `Fetcher` (`fetch.rs`) | `getblock <hash> 0` | least `load` count, height tie | `Lane::Sync` permits | hedge after 15 s; every source out → retry after 1 s | misanswer → benched 60 s (fixed) |
| `zaino-source` `TrafficBalancer` | `getrawtransaction <txid> 1` (GetTransaction, address txs) | P2C over peak-EWMA (10 s decay) | `Lane::Serve` permits | transient ×3 (250 ms, doubling), absent → next member | none |
| `zaino-source` `RpcClient` | every call | — | 3 lane semaphores, GCRA req + bytes | work-queue-full (`-1`) resent ×5, 500 ms | none |
| chainview `EndpointPoller` (`endpoint.rs`) | poll batch: `getblockchaininfo` + `getrawmempool true` + `getblockhash` ×2 (+ metadata /60 s); bytes batch | every trusted, own loop | `Lane::Control` | 1 s / 15 s streamed, 200 ms floor; 0.5 → 30 s ladder; 10 failures → `Down` | `Ewma` α 0.2 (telemetry only) |
| `IndexerWatch` (`indexer.rs`) | `ChainTipChange`, `MempoolChange` streams | each trusted with `indexer_address` | 1 gRPC channel | 0.5 → 30 s ladder | none |
| chainview `HeaderSync` (`headers.rs`) | `getblockheader <h> false` ×2000 batches | every validator in turn, sequential | `Lane::Sync` permits | stalled round → 5 s | rule failure → warned, skipped this round |
| chainview submission (`submit.rs`, `view.rs`) | `sendrawtransaction`; peer `push_isolated` | `Job`: uniform random, netgroup-distinct | `Lane::Control` | per `propagation_threshold`, `max_attempts` | none |
| zainod `upgrade_schedule` | poll batch at boot | each in order | `Lane::Control` | 1 → 30 s loop | none |
| `zaino-peers` `PeerNetwork` | `FindHeaders`, `BlocksByHash`, mempool ids/bytes | zebra `PeerSet` P2C (unattributed) | zebra's per-peer limits | 20 s timeout | not-asked answer → score 50 to zebra |

Three latency estimators, four back-off ladders, two blame rules, two pick rules, one retry layer
hidden under another (`RpcClient` resent `-1` up to 5 times inside each of `failover`'s 3 tries:
up to 18 sends per member). No caller knew another's load.

## 2. Reuse vs build

| Prior art | Evidence | Used as |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| zebra-network `PeerSet` (`peer_set/set.rs`) | own module doc: readiness ≠ request data is a mismatch; proposed fix = "one entity holds the peer set and metadata, each backpressure category a separate `Service`" | the shape of this design; `PeerSet` kept for crawl, `inv`, broadcast |
| Envoy priority levels / gRFC A50 outlier detection (external, not vendored) | lower priority gets traffic only when higher's healthy capacity runs out; ejection = base × times ejected | trusted tiers above peers; bench doubling |
| tower `balance::p2c` + `load::PeakEwma`, `hedge` percentiles, `retry::budget::TpsBudget`; `governor` GCRA | built in phase 1, deleted in the simplification | not used: a handful of trusted members, permits + transport byte pacing already bound zebrad's load (§10) |

Build: one pure core (`TrafficCore`) + one driver. Reuse no `tower::Service`: every answer needs
its sender (blame), and every hedge needs "anyone but him".

## 3. The one balancer

```text
  NFS ── block(hash, Tip|Bulk) ──┐                         ┌── Trusted P0 (ours)   RPC + push streams
  HeaderSync ── headers(..) ─────┤                         │
  mempool fold ── bytes(..) ─────┤   TrafficBalancer       ├── Trusted P1 (partner) RPC
  gRPC ── transaction(txid) ─────┼─▶ TrafficCore (pure) ───┤
  submission ── submit(entry) ───┤   members · lanes ·     └── Peers P2 (WorkPool, attributed)
  view ── poll_best(f) ──────────┤   hedge · failover · blame
                                 │
  ◀── Answered<T>{value, from, ticket} ── report(ticket, why) ──▶ bench, alarm
  ◀── Observation per trusted member (watch: poll reading) ──────▶ global snapshot
```

### Members

| Kind | Source | Priority | May answer |
| --------- | ------------------------------------- | -------------------------------------------------- | ---------------------------------------------- |
| `Trusted` | `[[trusted_validators]]`, fixed | `priority` key (0 default; 0 before 1 before …) | every class |
| `Peer` | WorkPool connection, joins / leaves | below every trusted | checkable only: headers, block bodies, mempool bytes |

Trusted-only (chainview §1 "only source" rows): `Poll` (listing, fees, holding), `Lookup` (mined tx
by id), the verdict `Submit`. A class never reaches a member kind not in its row (T2).

### Per-member state

```rust
struct Member {
    tier: u16,                 // trusted: priority; peers: below every trusted
    synced: Option<Synced>,    // last answered poll: Live | CatchingUp (peers: Live)
    failures: u32,             // consecutive NonDomain, every class (passive check)
    benched: Option<Bench>,    // misanswer; until = now + 60 s × 2^(times−1), ≤ 1 h
    latency: Duration,         // last reply's round trip (/statusz only, never a pick input)
    in_flight: PerLane<u32>,
    permits: Permits,          // max_connections − the poll's own, one reserved per lane
}
```

- `Health` (`Pending | Live | CatchingUp | Degraded | Down`): the poll is the active check
  (Pending → Live / CatchingUp), every request outcome the passive one (`failures`). 10
  consecutive → `Down`: only its poll probe goes, at the ladder's ceiling.
- `Bench` is orthogonal to health: a fast liar is `Live` and benched. Peers also carry a
  misbehaviour score (zcashd's); 100 → WorkPool drops it (`Left`).
- `Agreement` stays out: it is derived against the verified chain (snapshot's, §6).

### Lanes and classes

Per member, one permit is the poll's alone: it rides no lane and never waits behind an ask (it
drives tip following and finality vouching). Then three lanes, each with one reserved permit;
the rest of `max_connections` is shared by whoever waits, lanes dispatched in order (a freed
permit goes to the first waiting lane, FIFO within it). `MIN_CONNECTIONS` = poll + 3 reserves +
1 shared = 5.

| Lane | Classes | Why together |
| --- | --- | --- |
| `Control` | `Submit`, `Headers` | small, latency-critical, gate everything else (finality, admission) |
| `Interactive` | `TipBlock`, `Lookup`, `Bytes` | a waiting wallet or the tip |
| `Bulk` | `BulkBlock` | throughput; takes every idle shared permit, never a reserve |

A lane is admission and priority only. What differs per class is a property of the class (the
poll, §4, is no class: the core's own, never asked):

| Class | Asks | Members, order | Route | Hedge floor | On failure |
| ----------- | ------------------------------------------- | --------------------------------- | --------------------- | ---------- | ------------------------------ |
| `Submit` | `sendrawtransaction` | `Job`'s trusted entry | caller-chosen (privacy) | — | unanswered (`Job` decides) |
| `Headers` | `getblockheader` runs, `FindHeaders` | trusted: pinned to the claim's member; peers: any | pinned / least loaded | — | pinned: unanswered; peers: next |
| `TipBlock` | `getblock <hash> 0`, `BlocksByHash` | trusted by tier, then peers | least loaded within tier | 2 s | next member; all out → 1 s |
| `Lookup` | `getrawtransaction <txid> 1` | trusted by tier | least loaded, absent → next | 1 s | next member, then unanswered |
| `Bytes` | `getrawtransaction 0` batch, `TransactionsById` | listers first within tier, then least loaded | `Prefer` | — | next member |
| `BulkBlock` | `getblock <hash> 0` | trusted by tier, then peers | least loaded within tier | 15 s | next member; all out → 1 s |

- **Pick** (T6): eligible members with room → best tier → the route's preferred member, else the
  least in flight (ties: configured order). A first attempt goes to a lower tier only when no
  eligible higher-tier member has a permit now; hedges follow the same rule.
- **Eligible** = kind allowed, not benched, not `Down`, not tried this round; catching up (by its
  last answered poll, not `Health`: failing + catching up reads `Degraded`) excluded from `Bytes`
  and `Lookup` (no mempool, lagging chain).
- **Control progress** (T7): a `Headers` or `Submit` ask on a member waits for at most one
  completion that frees a control-reserve or shared permit there (its lane is dispatched first).
- **Polls never wait** (T10): a due poll goes out at once on its own permit, whatever the lanes
  hold.
- Peers: one request in flight each (a zebra peer connection serves one).
- Load on zebrad is bounded by `max_connections` (here) and `max_mib_per_sec` (byte pacing in
  the transport, `LinkLimits`). There is no request-rate limit.

### Hedge, failover, blame: one policy

```text
  ask ─▶ pick (tier, least loaded) ─▶ send ──┬─ value ───────▶ Answered{from, ticket} ─▶ caller checks
                                             │                              └─ fails → report(ticket, why)
                                             ├─ Domain(absent) ─▶ next member (no blame)  ─▶ bench member, re-ask
                                             ├─ NonDomain ─▶ failures += 1, next member
                                             └─ silent past the class's hedge floor ─▶ second member, first kept
  every eligible member tried ─▶ round over: retry after 1 s (blocks) │ Unanswered (others)
```

- **Bounded by members, not a budget**: a round asks each member once (T5); blocks retry the
  round after 1 s, everything else ends unanswered. No transport-level resends.
- **Hedge** = fixed per-class floor from the latest send (`TipBlock` 2 s, `Lookup` 1 s,
  `BulkBlock` 15 s). Winner = first `Ok`; the rest are dropped (future dropped, permit returned).
- **Misanswer** = the caller's check failed (`check_block`, header rule H8, txid recomputed from
  bytes). The value never reaches anyone else; `report` benches `from`, logs, counts
  `zaino_traffic_misanswers_total{member,class}`, and the caller's re-ask excludes `from`.
  Header from the future (H7) and orphan runs are **not** misanswers.
- Absent is never blame (a lagging validator); a `Lookup` answers the first value, else a
  transport failure (it may have held it), else the last absence.

## 4. Unified polling

One loop per trusted member, inside the balancer, replaces `EndpointPoller`, `PollWaker`,
`IndexerWatch`'s wiring and `upgrade_schedule`:

```text
  wake: interval (1 s; 15 s with both push streams up) │ push event │ stream edge
    ─▶ ≥ 200 ms since last ─▶ own permit ────▶ batch: getblockchaininfo + getrawmempool true
                                                       + getblockhash <poll_best()> (+ metadata /60 s)
                                             ─▶ Observation → watch (per member) ; health, latency
```

- **Holder question** rides the batch: `getblockhash` at `poll_best()` (the view's best height),
  read as each poll starts. A new best wakes no poller (an answer at a moved best = one stale
  fact, re-asked next poll).
- **Mempool delta**: the poll lists; the consumer's diff (its own last listing per member) names
  what it lacks; `bytes(..)` fetches those, once across members (affinity = the listers).
- **Header sync** waits on observations instead of the view: a claim off the verified best →
  `headers(Pinned(member), heights)`.
- Poll failure → `Observation { polled: Err }`: that member holds nothing until it answers
  again; health and ladder are the balancer's. Only an answered poll consumes the metadata
  refresh. Boot's upgrade schedule = the first *answered* poll, `CatchingUp` included.

## 5. API

```rust
pub struct TrafficBalancer<S> { /* Arc<Shared>: Mutex<TrafficCore + ask mailboxes>, sources, watches */ }
pub struct TrafficDriver<S> { /* the core's clock, every poll, peer join/leave: one task */ }

pub enum MemberId { Trusted(ValidatorId), Peer(PeerId) }
pub struct ValidatorId(u8);          // < ValidatorId::MAX (64), configured order
pub struct PeerId(pub u64);          // WorkPool connection, never reused
pub struct Limits { .. }             // Limits::new(max_connections ≥ 5, _) (2nd arg ignored, §9)
pub struct Trusted<S> { pub source: Arc<S>, pub priority: u8, pub limits: Limits }

pub struct Answered<T> { pub value: T, pub from: MemberId, pub ticket: Ticket }
pub struct Ticket { ask: AskId, member: MemberId, class: Class }
pub enum Urgency { Tip, Bulk }
pub enum HeaderAsk {
    Pinned { member: ValidatorId, heights: Vec<Height> },
    Peers { locator: Vec<BlockHash>, stop: Option<BlockHash> },
}
pub struct Unanswered<E> { pub last: Option<QueryError<E>> }   // None = nobody eligible to ask
pub enum Push { Changed, Link(bool) }

pub trait PeerTransport: Send + Sync + 'static {  // zainod implements over zaino-peers
    fn headers(&self, peer: PeerId, locator: Vec<BlockHash>, stop: Option<BlockHash>)
        -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>>;
    fn block(&self, peer: PeerId, hash: BlockHash) -> BoxFuture<'static, Result<Block, NonDomainError>>;
    fn transactions(&self, peer: PeerId, ids: Vec<TransactionId>)
        -> BoxFuture<'static, Result<Vec<Option<Vec<u8>>>, NonDomainError>>;
    fn joined_left(&self) -> BoxStream<'static, Membership>;  // Joined(PeerId) | Left(PeerId)
}

impl<S: ChainDataSource> TrafficBalancer<S> {
    pub fn new(trusted: Vec<Trusted<S>>, peers: Option<Arc<dyn PeerTransport>>)
        -> (Self, TrafficDriver<S>);
    pub async fn block(&self, hash: BlockHash, urgency: Urgency) -> Answered<Block>;  // pending until served
    pub async fn headers(&self, ask: HeaderAsk)
        -> Result<Answered<BlockLinks>, Unanswered<GetAtHeightError>>;
    pub async fn bytes(&self, listed: Vec<MempoolListed>, prefer: Vec<MemberId>)
        -> Result<Answered<RawMempoolTransactions>, Unanswered<GetRawMempoolTransactionError>>;
    pub async fn transaction(&self, txid: TransactionId)
        -> Result<Answered<TransactionResponse>, Unanswered<GetTransactionError>>;
    pub async fn submit(&self, member: ValidatorId, raw: Vec<u8>)
        -> Result<Answered<TransactionId>, Unanswered<SendRawTransactionError>>;
    pub fn entries(&self) -> Vec<ValidatorId>;   // trusted entries: live, not benched
    pub fn poll_best(&self, best: impl Fn() -> Option<Height> + Send + Sync + 'static);
    pub fn pushed(&self, member: ValidatorId, push: Push);   // IndexerWatch callbacks
    pub fn observe(&self, member: ValidatorId) -> watch::Receiver<Option<Arc<Observation>>>;
    pub fn members(&self) -> watch::Receiver<Arc<MemberTable>>;  // /statusz, metrics
    pub fn report(&self, ticket: Ticket, why: &(dyn std::error::Error + 'static));
}

pub struct Observation {
    pub member: ValidatorId, pub at: Instant, pub asked: Option<Height>,
    pub polled: Result<PollReading, NonDomainError>, pub streaming: bool, pub health: Health,
}
```

- Dropping an ask's future abandons it (its sends dropped, permits back); no cancel API.
- `headers(HeaderAsk)`: a peer is asked by locator, not by height (the balancer holds no chain).
- `bytes`: one batch from one member; `prefer` orders members *within* the best tier with room.
- `submit(member, raw)`: the verdict class only, trusted only. A peer push goes to an *address*
  (`push_isolated`, never a WorkPool member), so it stays with `Job` and `zaino-peers`.
- `pushed(member, Push)`: zainod wires `IndexerWatch::run`'s callbacks to it, keeping gRPC
  connects out of this crate.
- `Observation.health` = the member's health right after that poll (the view folds `Degraded`
  vs `Down` without racing the `MemberTable`).
- No `Policy`: lanes, hedge floors and cadences are constants (an unused knob is a bug).

| Caller | Uses |
| ------------------------- | ---------------------------------------------------------------------------- |
| NFS driver | `block(hash, Tip/Bulk)` → `check_block`; `Err` → `report` + re-ask |
| `HeaderSync` | `headers(Pinned(i) / Peers, ..)`; rule failure → `report` |
| mempool fold / snapshot | `observe(i)` per member; `bytes(..)` for the delta |
| submission | `entries()` → `Job` → `submit(entry, raw)` |
| gRPC `GetTransaction`, address txs | `transaction(txid)` (class `Lookup`) |
| zainod boot | first answered observation's `info`; one `TrafficBalancer` handed to all |

## 6. Interface with the global snapshot

| Owned by the balancer | Owned by the snapshot |
| -------------------------------------------------------- | -------------------------------------------------------------- |
| who is reachable, last latency, in flight, benched, health | tip, holders, agreement, mempool sightings, lightd info |
| when to poll, what a poll costs, failover, hedges | what to ask (`poll_best`), what bytes it lacks |
| `Observation` (raw reading, per member, latest-only) | folding observations; `/statusz` joins both tables |

- One-way types: the snapshot imports `zaino_traffic::{Observation, MemberTable, ValidatorId}`
  only; the balancer imports nothing of the snapshot.
- Latest-only (`watch`) is safe: each poll replaces the last one's facts, and the mempool diff
  runs against the consumer's last-seen listing, not the previous poll. A slow fold never stalls
  a poll.

## 7. Invariants

| ID | Invariant | Where |
| --- | -------------------------------------------------------------------------------------------------- | ---------------------------------- |
| T1 | per member: Σ in flight ≤ `max_connections`; a lane's reserve never taken by another lane | `check`, `Permits` |
| T2 | a class reaches only member kinds its row allows (trusted-only never reaches a peer) | `check` over in-flight, model |
| T3 | no new send to a benched or `Down` member (except its poll probe) | `check` (event stamps), model |
| T4 | every ask ends once: answered, unanswered or abandoned; a hedge loser is never delivered | `check`, model |
| T5 | a round never asks one member twice; a re-ask after `report` excludes the reported member | `check`, model |
| T6 | a send (first or hedge) goes to the best tier with room, then the preferred, then the least loaded | `check` (recorded tier), model (exact pick) |
| T7 | no ready ask waits while an eligible member has room for its lane; a shared permit never goes to a lower lane while a higher lane's ask waits for that member | model (own lanes + permits) |
| T8 | every misanswer is charged to the member that sent it, and only to it | `Ticket`, model |
| T9 | an ask an honest member may serve is answered; with a faster honest member, within hedge floor + one round | model at quiescence, bound matrix |
| T10 | a trusted member is polled ≤ once per 200 ms and ≥ once per interval (ladder while failing), never delayed by an ask | `check`, model, full-lanes test, driver test |

## 8. Tests

- **Core** (`zaino-traffic/src/core.rs`): `TrafficCore::step(&mut self, Input, now) ->
  Vec<Output>`, time as input, `check()` after every step in tests and debug builds;
  preconditions as named `assert!`s.
- **Model** (`core/model.rs`, proptest): members `Honest`, `Slow`, `Lying` (wrong value),
  `Lagging` (absent), `Flapping`, `Down`, `Silent`; peers joining and leaving; random asks of
  every class and route, abandons, reports, clock advances. The oracle keeps its own lanes,
  permits and rounds: exact pick per send (T6), lane priority per send and work conservation
  after every step (T7); at quiescence T9, T10, every lying member benched. Swarm-style: whole
  member kinds off per case. A bound matrix (tier 0 silent / slow / lagging / down, tier 1
  honest) pins each class's end time to its hedge floor + one round; with every lane permit held
  by silent asks, a due poll still goes out at once (T10). `PROPTEST_CASES=1000` loop ≥ 3 min
  after any change.
- **Fire drills** (`core/fire_drills.rs`): one planted bug per `check()` assertion and
  precondition.
- **Driver** (`tests.rs`, paused single-thread runtime): `MockValidator` members with per-port
  latency and failure injection: a lookup storm never delays a poll, a merkle-lying member is
  benched and its block served by another, a hedge beats a 20 s stall, a push stream wakes the
  poll within 200 ms.

## 9. Migration (1–7 and 9 done, 8 open)

1. `zaino-traffic`: `Observation`, `MemberTable`; `EndpointPoller` emits `Observation`.
1. `TrafficCore` + model + fire drills, no users.
1. Driver over trusted `ZebraRpcAdapter` members; `Lookup`: gRPC `GetTransaction` + address txs.
1. NFS on `block(..)`; `report` on `check_block` failure.
1. Lanes replace the transport's semaphores; transport resends deleted.
1. Polling moves in: pollers, push streams, `poll_best`, `bytes`.
1. `HeaderSync` and submission on `headers(..)` / `submit(..)`.
1. Peers as members via `PeerTransport` over the WorkPool: headers, blocks,
   bytes.
1. Docs, changesets.

Open: `Limits::new`'s second argument (the deleted request rate) is ignored; drop it once
zaino-chainview's tests (`network_model.rs`, `tests.rs`) call `Limits::new(n)`.

## 10. Decisions

1. **Crate.** `zaino-traffic` above `zaino-source` with `PeerTransport` as a port: keeps
   zebra-network out of everything but zainod; `zaino-source` stays one validator's RPC.
1. **Trusted tiers.** `priority` on `[[trusted_validators]]`, default 0: "ours on the LAN" vs
   "partner's across an ocean" is operator knowledge.
1. **Trusted first everywhere but `Submit`** (privacy): peers take overflow and hedges.
1. **Hedge delay = fixed class floor.** A latency percentile per member per class bought little
   with a handful of members and cost a histogram each.
1. **Pick = least in flight within the tier.** PeakEWMA + power-of-two-choices balanced a large
   pool; a few trusted members need only load and their configured order.
1. **No request-rate limit, no retry budget.** A GCRA charged a 2000-header ask 2000 tokens
   (every class blocked seconds per batch) and polls unconditionally (pinned asks wedged below
   ~4 rps). Permits + byte pacing already bound zebrad; a round asks each member once, so retries
   are bounded by the member count.
1. **Three lanes, one reserve each, the poll outside them.** Seven classes with reserve +
   ceiling left `Headers` with no reserve behind bulk; the control lane gives submit and headers
   their own permit. The poll keeps a permit of its own: it drives tip following and finality
   vouching, so sharing the control lane (a poll waiting behind a `Headers` batch, gaps of 43 s
   in the model) is a regression.
1. **Benching the last trusted member.** Bench anyway, alarm: a trusted member serving a block
   that fails its merkle root is broken or intercepted; finality pausing is the correct answer.
1. **Observation delivery.** `watch` (latest), §6.
1. **Abandon.** Dropping an ask's future abandons it: NFS drops wants off the best chain by
   dropping futures, no cancel API.
