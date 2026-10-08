# TrafficBalancer: one way Zaino talks to validators and peers

Status: **phase 2 built** (2026-10-07): every trusted-validator request goes through
`zaino-traffic` (§9 steps 1–7 and 9); peers as members (step 8) wait for the WorkPool. §5 lists
what changed while building each phase. Builds on [chainview.md](chainview.md) §7–§9,
[verified-chain.md](verified-chain.md) §6–§7 and §10, [pipeline.md](pipeline.md). Boundary with
`global-snapshot.md` in §6.

## 1. Today: five schedulers over the same validators

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
hidden under another (`RpcClient` resends `-1` up to 5 times inside each of `failover`'s 3 tries: up to 18 sends per member). No caller knows
another's load: a bulk-sync burst, a wallet `GetTaddressTransactions` fan-out and the poll each
see their own lane, never the member.

## 2. Reuse vs build

| Prior art | Evidence | Used as |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| tower `balance::p2c` + `load::PeakEwma` (0.5.3) | P2C over `Load`; cost = EWMA RTT × pending; peak jump, decay toward mean (`load/peak_ewma.rs`) | **the rule**, not the `Service`: `Service::poll_ready` cannot depend on the request (class) |
| zebra-network `PeerSet` (`peer_set/set.rs`) | own module doc: readiness ≠ request data is a mismatch; proposed fix = "one entity holds the peer set and metadata, each backpressure category a separate `Service`" | the shape of this design; `PeerSet` kept for crawl, `inv`, broadcast (verified-chain §6) |
| zebra `LoadTrackedClient` | `PeakEwma::new(EWMA_DEFAULT_RTT = timeout + 1 s, decay 200 s)` | default RTT for **peers** (pessimistic); trusted keep 30 ms (optimistic: tried first) |
| tower `hedge` | re-sends through the **same** inner service after a rotating-histogram percentile, `min_data_points` | the percentile rule; not the middleware (hedge must exclude the first member) |
| tower `retry::budget::TpsBudget` | deposit per call, withdraw per retry, `ttl`, `min_per_sec`, `retry_percent`; reads `tokio::time::Instant::now()` itself | its rates as a token bucket (10 % per first attempt, 1/s, cap 10; core takes time as input) |
| `governor` (in tree) | GCRA; pluggable `clock::Clock` (`FakeRelativeClock`); no non-consuming check | not used in the core: a pick compares members' headroom before charging one, so the core's GCRA is ~15 lines; byte rate stays per body chunk in transport |
| Envoy priority levels / gRFC A50 outlier detection (external, not vendored) | lower priority gets traffic only when higher's healthy capacity runs out; ejection = base × times ejected | trusted tiers above peers; bench doubling |

Build: one pure core (`TrafficCore`) + one driver. Reuse no `tower::Service`: every answer needs
its sender (blame), and every hedge needs "anyone but him".

## 3. The one balancer

```text
  NFS ── block(hash, Tip|Bulk) ──┐                         ┌── Trusted P0 (ours)   RPC + push streams
  HeaderSync ── headers(..) ─────┤                         │
  mempool fold ── bytes(..) ─────┤   TrafficBalancer       ├── Trusted P1 (partner) RPC
  gRPC ── transaction(txid) ─────┼─▶ TrafficCore (pure) ───┤
  submission ── push(entry) ─────┤   members · classes ·   └── Peers P2 (WorkPool, attributed)
  snapshot ── ask_each_poll(h) ──┤   hedge · retry · blame
                                 │
  ◀── Answered<T>{value, from, ticket} ── report(ticket, why) ──▶ bench, score, alarm
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
    kind: Kind,                          // Trusted { id, priority } | Peer { id }
    health: Health,                      // Pending | Live | CatchingUp | Degraded | Down
    failures: u32,                       // consecutive NonDomain, every class (passive check)
    benched: Option<Bench>,              // misanswer; until = now + 60 s × 2^(times−1), ≤ 1 h
    latency: PeakEwma,                   // one estimator (cost = estimate × (in flight + 1))
    hedge_at: PerClass<Percentile>,      // rotating histogram, p95, ≥ 10 samples
    in_flight: PerClass<u32>,
    permits: Permits,                    // max_connections, reserved + ceiling per class
    rate: Gcra,                          // max_requests_per_sec, charged per call (batch N = N)
}
```

- `Health` = chainview's `EndpointState`, moved: the poll is the active check (Pending → Live /
  CatchingUp), every request outcome the passive one (`failures`). 10 consecutive → `Down`: only
  its `Poll` probe goes, at the ladder's ceiling.
- `Bench` is orthogonal to health: a fast liar is `Live` and benched. Peers also carry the
  verified-chain §6 score; 100 → WorkPool drops it (`Left`).
- `Agreement` stays out: it is derived against the verified chain (snapshot's, §6).

### Classes: priorities and budgets replace lanes

Per member, a freed permit goes to the highest waiting class under its ceiling; a class's reserve
is never taken by another. Lanes = the special case ceiling = reserve.

| Class (prio) | Asks | Members, order | Route | Reserve / ceiling (of 32) | Hedge | On failure |
| ------------ | ------------------------------------------- | --------------------------------- | --------------------- | ------------------------- | -------------------- | ------------------------------ |
| `Poll` 0 | poll batch, then bytes of new listings | each trusted | each, own cadence | 1 / 1 | — | ladder 0.5 → 30 s |
| `Submit` 1 | `sendrawtransaction` | `Job`'s trusted entry | caller-chosen (privacy) | 1 / 2 | — (`Job`'s timer) | `Job` decides |
| `TipBlock` 2 | `getblock <hash> 0`, `BlocksByHash` | trusted by tier, then peers | P2C within tier | 1 / 4 | p95, floor 2 s | next member; all out → 1 s |
| `Headers` 3 | `getblockheader` runs, `FindHeaders` | trusted: pinned to the claim's member; peers: P2C | pinned / P2C | 0 / 4 | — | round retried after 5 s |
| `Lookup` 4 | `getrawtransaction <txid> 1` | trusted by tier | P2C, absent → next | 1 / 8 | p95, floor 1 s | transient → next, budgeted |
| `Bytes` 5 | `getrawtransaction 0` batch, `TransactionsById` | listers / announcers, then P2C | affinity (zebra `route_inv`) | 0 / 2 | — | next member |
| `BulkBlock` 6 | `getblock <hash> 0` | trusted by tier, then peers | P2C within tier | 1 / rest | p95, floor 15 s | next member; all out → 1 s |

- **Tier spill** (T6): a first attempt goes to a lower tier only when no eligible higher-tier
  member has a permit and rate headroom now. Hedges follow the same rule (an untried member of
  the best tier with room).
- **Eligible** = kind allowed, not benched, not `Down`, not tried this round; catching up (by its
  last answered poll, not `Health`: failing + catching up reads `Degraded`, a model find)
  excluded from `Bytes` and `Lookup` (no mempool, lagging chain).
- Defaults scale with `max_connections`; rest = `max_connections` − every other reserve;
  `MIN_CONNECTIONS` = Σ reserves + 1 = 6 (today 4: bulk gets its own reserve, never starved).
  The + 1 is the model's find: at Σ reserves = 5, `Headers` and `Bytes` (no reserve) have a
  ceiling of 0 and never run.
- Peers: one request in flight each (a zebra peer connection serves one).

### Hedge, retry, blame: one policy

```text
  ask ─▶ pick (tier, P2C) ─▶ send ──┬─ value ─────────────▶ Answered{from, ticket} ─▶ caller checks
                                    │                                   └─ fails → report(ticket, why)
                                    ├─ Domain(absent) ─▶ mark tried, next member (no blame)  ─▶ bench member, re-ask
                                    ├─ NonDomain ─▶ failures += 1, next member if budget allows
                                    └─ silent past hedge_at ─▶ second member (budget), first kept
  every eligible member tried ─▶ round over: retry after 1 s (blocks) │ Unanswered (lookups)
```

- **One retry budget per member set**: retries + hedges ≤ 10 % of first attempts + 1/s over a
  10 s window (`TpsBudget`'s rule, ported). No transport-level resends (decision 6).
- **Hedge winner** = first `Ok`; the loser is cancelled (its future dropped, permit returned),
  its latency sample discarded (a censored sample would lower its estimate).
- **Misanswer** = the caller's check failed (`check_block`, header rule H8, txid recomputed from
  bytes). The value never reaches anyone else; `report` benches `from`, logs, counts
  `zaino_traffic_misanswers_total{member,class}`, and the caller's re-ask excludes `from`.
  Header from the future (H7) and orphan runs are **not** misanswers.
- Absent is never blame (a lagging validator); a `Lookup` answers the first value, else a
  transport failure (it may have held it), else the last absence: `failover` semantics, kept.

## 4. Unified polling

One loop per trusted member, inside the balancer, replaces `EndpointPoller`, `PollWaker`,
`IndexerWatch`'s wiring and `upgrade_schedule`:

```text
  wake: interval (1 s; 15 s with both push streams up) │ push event │ stream edge │ new questions
    ─▶ ≥ 200 ms since last ─▶ Poll permit ─▶ batch 1: getblockchaininfo + getrawmempool true
                                                      + getblockhash per asked height (+ metadata /60 s)
                                           ─▶ batch 2: bytes of txids the consumer lacks (≤ 100 / 8 MiB)
                                           ─▶ Observation → watch (per member) ; health, latency
```

- **Holder questions** ride batch 1: the snapshot sets them with `ask_each_poll([boundary, best])`
  (`Holders::asked`); setting new heights wakes every poller. No separate holder class.
- **Mempool delta**: batch 1 lists; the consumer's diff (its own last listing per member) names
  what it lacks; batch 2 fetches those, once across members (`Bytes`, affinity = this member).
  `MempoolChange` events carry the txid (zebra `indexer.proto`): a later step fetches on the event
  and skips batch 2; the listing stays the truth.
- **Header sync** waits on observations instead of the view: a claim off the verified best →
  `headers(Pinned(member), heights)`; each run's last header → `Holders::served` (unchanged).
- Poll failure → `Observation { polled: Err }`: the snapshot forgets that member's facts
  (holders' rule); health and ladder are the balancer's.

## 5. API

```rust
// zaino-traffic (as built, phase 1)
pub struct TrafficBalancer<S> { /* Arc<Shared>: Mutex<TrafficCore + ask mailboxes>, sources, watches */ }
pub struct TrafficDriver<S> { /* the core's clock, every poll, peer join/leave: one task */ }

pub enum MemberId { Trusted(ValidatorId), Peer(PeerId) }
pub struct ValidatorId(u8);          // < ValidatorId::MAX (64), configured order
pub struct PeerId(pub u64);          // WorkPool connection, never reused
pub struct Limits { .. }             // Limits::new(max_connections ≥ 6, max_requests_per_sec)
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
    /// pending until served; drop = abandon
    pub async fn block(&self, hash: BlockHash, urgency: Urgency) -> Answered<Block>;
    pub async fn headers(&self, ask: HeaderAsk)
        -> Result<Answered<BlockLinks>, Unanswered<GetBlockError>>;
    pub async fn bytes(&self, listed: Vec<MempoolListed>, prefer: Vec<MemberId>)
        -> Result<Answered<RawMempoolTransactions>, Unanswered<GetRawMempoolTransactionError>>;
    pub async fn transaction(&self, txid: TransactionId)
        -> Result<Answered<TransactionResponse>, Unanswered<GetTransactionError>>;
    pub async fn submit(&self, member: ValidatorId, raw: Vec<u8>)
        -> Result<Answered<TransactionId>, Unanswered<SendRawTransactionError>>;
    pub fn entries(&self) -> Vec<ValidatorId>;   // trusted entries: live, not benched
    pub fn ask_each_poll(&self, heights: Vec<Height>);
    pub fn pushed(&self, member: ValidatorId, push: Push);   // IndexerWatch callbacks
    pub fn observe(&self, member: ValidatorId) -> watch::Receiver<Option<Arc<Observation>>>;
    pub fn members(&self) -> watch::Receiver<Arc<MemberTable>>;  // /statusz, metrics
    pub fn report(&self, ticket: Ticket, why: &(dyn std::error::Error + 'static));
}

pub struct Observation {
    pub member: ValidatorId, pub at: Instant, pub asked: Vec<Height>,
    pub polled: Result<PollReading, NonDomainError>, pub streaming: bool,
}
```

Changed while building (phase 1):

- `headers(HeaderAsk)`: a peer is asked by locator, not by height (the balancer holds no chain to
  turn heights into hashes).
- `bytes`: takes the listing entries (`get_raw_mempool_transactions` batches by `encoded_len`)
  and returns one batch from one member; per-item re-asks across members wait for step 6. `prefer`
  orders members *within* the best tier with room (T6 holds for every send).
- `submit(member, raw)`: the verdict class only, trusted only. A peer push goes to an *address*
  (`push_isolated`, never a WorkPool member), so it stays with `Job` and `zaino-peers`;
  `PeerTransport` has no `push_isolated`, `entries()` no `tip`.
- `Unanswered::last` is an `Option`: a pinned ask to a benched or `Down` member never sends.
- No `Policy`: classes, hedges, budget and cadences are constants (an unused knob is a bug).
- `pushed(member, Push)` instead of the driver owning push streams: zainod wires
  `IndexerWatch::run`'s callbacks to it (step 6), keeping gRPC connects out of this crate.

Changed while migrating (phase 2):

- `Observation.health` = the member's health right after that poll: the chain view folds
  `Degraded` vs `Down` without racing the published `MemberTable` (`MemberRow.failures` and
  `Health::label` feed `/statusz` and the `endpoint_state` gauge; `Unanswered` displays).
- Only an answered poll consumes the metadata refresh (a failing member's first answer reads it).
- Boot's upgrade schedule = the first *answered* poll, `CatchingUp` included (a syncing zebrad
  holds the network's schedule; waiting for `Live` would hold boot until it reaches the tip).
- The transport keeps per-chunk byte pacing only (`LinkLimits { max_connections,
  max_bytes_per_sec }`); request rate = the core's GCRA alone.
- A validator with no mempool method (`-32601`) folds as `CatchingUp` (chain counted, no
  sightings), as the core reads it; formerly `Down` at once.
- The view's NFS-facing contract kept, its transport switched: `ChainView::new(addresses,
  balancer, depth)`, `ObservationFold` (one task, a loop per member), `ValidatorId` replaces
  `EndpointIndex`. Holders gained `PollStamp`: a header run counts only under the poll it was
  read under.
- Submission entries = `entries()` (live, not benched): no "every member down → try them all"
  fallback.
- NFS: the core names each fetch and its end (`Output::{Fetch, Abandon}`); the driver drops the
  abandoned future.
- Bytes answers are not yet checked against their txid (trusted only today); the check lands with
  peers (step 8).

| Caller | Today | After |
| ------------------------- | -------------------------------------------------- | ---------------------------------------------------------------------------- |
| NFS driver | `Output::Fetch { from }` → `sources[from]` | `Output::Fetch { at, record }` → `block(hash, Tip/Bulk)` → `check_block` → `Input::Body`; `Err` → `report` + re-ask |
| `HeaderSync` | `sources[i].get_block_links` | `headers(Pinned(i) / Peers, ..)`; rule failure → `report` |
| mempool fold / snapshot | `EndpointPoller` reports into `ChainViewCore` | `observe(i)` per member; `bytes(..)` for the delta |
| submission | `sources[i].send_raw_transaction`, `peers.push` | `entries()` → `Job` → `submit(entry, raw)` |
| gRPC `GetTransaction`, address txs | `TrafficBalancer<S>::failover` | `transaction(txid)` (class `Lookup`) |
| zainod boot | `upgrade_schedule` loop; `laned(..)` ×3 | first `Live` observation's `info`; one `TrafficBalancer` handed to all |

**Deleted**: `zaino-nfs` `Fetcher` scheduling (`Source`, `load`, `BENCH`, `HEDGE`, `RETRY`,
`pick`, `Output::{Unserved, Misanswered}`, `Input::Body.from`, `sources`; `check_block` stays);
`zaino-source` `balance.rs`, `Lane`, lane semaphores, `ZebraRpcAdapter::on`, `RpcClient`
`max_retries`; chainview `EndpointPoller`, `PollWaker`, cadence + ladder consts, `Ewma`,
`EndpointState` (moved), `HeaderSync.sources` and its per-validator loop; `view.rs` push paths;
zainod `upgrade_schedule`, `laned`, poller/watch spawning.

## 6. Interface with the global snapshot

| Owned by the balancer | Owned by the snapshot |
| -------------------------------------------------------- | -------------------------------------------------------------- |
| who is reachable, how fast, in flight, benched, health | tip, holders, agreement, mempool sightings, lightd info |
| when to poll, what a poll costs, retries, hedges | what to ask (`ask_each_poll`), what bytes it lacks |
| `Observation` (raw reading, per member, latest-only) | folding observations; `/statusz` joins both tables |

- One-way types: the snapshot imports `zaino_traffic::{Observation, MemberTable, ValidatorId}`
  only; the balancer imports nothing of the snapshot.
- Latest-only (`watch`) is safe: each poll replaces the last one's facts, and the mempool diff
  runs against the consumer's last-seen listing, not the previous poll. A slow fold never stalls
  a poll.
- **Both proceed now**: migration step 1 adds `Observation` and makes today's `EndpointPoller`
  emit it; the snapshot builds on that type while the balancer replaces the poller behind it.

## 7. Invariants

| ID | Invariant | Where |
| --- | -------------------------------------------------------------------------------------------------- | ---------------------------------- |
| T1 | per member: Σ in flight ≤ `max_connections`; per class ≤ ceiling; reserves never borrowed | `check`, `Permits` asserts |
| T2 | a class reaches only member kinds its row allows (trusted-only never reaches a peer) | `check` over in-flight, model |
| T3 | no new send to a benched or `Down` member (except its `Poll` probe) | `check` (event stamps), model |
| T4 | every ask ends once: answered, unanswered or abandoned; a hedge loser is never delivered | `check`, model |
| T5 | a round never asks one member twice; a re-ask after `report` excludes the reported member | `check`, model |
| T6 | a send (first or hedge) goes to tier n+1 only if no eligible tier ≤ n member had a permit and rate | `check` (recorded pick), model |
| T7 | retries + hedges ≤ budget over the window | `check`, model |
| T8 | every misanswer is charged to the member that sent it, and only to it | `Ticket`, model |
| T9 | an ask with an eligible honest live member is answered within hedge + one round (subsumes P5) | model at quiescence, driver test |
| T10 | a trusted member is polled ≤ once per 200 ms and ≥ once per interval (ladder while failing) | model, paused-clock driver test |

## 8. Tests

- **Core** (`zaino-traffic/src/core.rs`): `TrafficCore::step(&mut self, Input, now: Instant) -> Vec<Output>`, seeded RNG as input, `check()` after every step in tests and debug builds;
  preconditions as named `assert!`s (`"an answer for an ask in flight"`).
- **Model** (`core/model.rs`, proptest): members `Honest`, `Slow`, `Lying` (wrong value),
  `Lagging` (absent), `Flapping`, `Down`, `Silent`; peers joining and leaving; random asks of
  every class and urgency, abandons, reports, clock advances. Oracle: a naive scheduler's
  eligibility sets. After each step: T1–T8; at quiescence: T9, T10, every lying member benched,
  no lie delivered as accepted (the model plays the caller's check). Swarm-style: whole member
  kinds off per case. `PROPTEST_CASES=1000` loop ≥ 3 min after any change (as persistence).
- **Fire drills** (`core/fire_drills.rs`): one planted bug per `check()` assertion and
  precondition, pattern of `zaino-nfs/src/fetch.rs`'s `every_fetch_check_fires_on_its_planted_bug`.
- **Driver** (`tests.rs`, paused single-thread runtime): `MockValidator` members (`mock-chain.md`)
  with per-port latency and failure injection; one scenario per story: a wallet storm never delays a poll (T1 + T10),
  a merkle-lying member is benched and its block served by another, a hedge beats a 20 s stall,
  a push stream wakes the poll within 200 ms.
- **Consumers keep theirs**: NFS model loses source kinds (bodies arrive late, never, or not at
  all, from "the balancer"); chainview's `network_model` drives the real balancer.

## 9. Migration (each step green, live suite after 4 and 7; 1–7 and 9 done, 8 open)

1. `zaino-traffic`: `Observation`, `MemberTable`; today's `EndpointPoller` emits `Observation`
   (global-snapshot unblocked here).
1. `TrafficCore` + model + fire drills, no users.
1. Driver over trusted `ZebraRpcAdapter` members; `Lookup`: gRPC `GetTransaction` + address txs.
   Delete `balance.rs`.
1. NFS on `block(..)`: delete `Fetcher` scheduling, `Unserved`, `Misanswered`; `report` on
   `check_block` failure.
1. Classes replace lanes: one permit pool per member in the core; delete `Lane`, `on`, transport
   resends.
1. Polling moves in: pollers, push streams, `ask_each_poll`, `bytes`; delete `EndpointPoller`,
   `PollWaker`, `upgrade_schedule`.
1. `HeaderSync` and submission on `headers(..)` / `submit(..)`.
1. Peers as members via `PeerTransport` over the WorkPool (verified-chain §6): headers, blocks,
   bytes; chainview §9's table replaced by §3's.
1. Docs: `zaino-traffic/usage.md`, README index, chainview §7/§9 and nfs §6 point here; changesets.

## 10. Open decisions

1. **Crate.** New `zaino-traffic` above `zaino-source` (RPC transport) with `PeerTransport` as a
   port, vs growing `zaino-source`. **Recommend new crate**: keeps zebra-network (tower 0.4) out
   of everything but zainod, and `zaino-source` stays one validator's RPC.
1. **Trusted tiers.** `priority` on `[[trusted_validators]]`, default 0. **Recommend yes**: "ours
   on the tailnet" vs "partner's across an ocean" is operator knowledge P2C learns only by losing
   requests.
1. **Peers-first exceptions.** chainview §9 sends tip blocks and mempool bytes to peers first (to
   shrink trusted load). **Recommend trusted first everywhere but `Submit`** (privacy): the
   operator's ask; peers take overflow and hedges. Revisit with fleet numbers.
1. **Hedge delay.** Fixed (NFS 15 s) vs per-class p95. **Recommend p95 with a class floor**
   (tower's rule): a 15 s fixed hedge at the tip costs 20 % of a 75 s block interval.
1. **Benching the last trusted member.** Envoy caps ejection at a percentage. **Recommend bench
   anyway**, alarm: a trusted member serving a block that fails its merkle root is broken or
   intercepted; finality pausing is the correct answer.
1. **Transport resends** (`RpcClient` ×5 on `-1`). **Recommend delete**: one budget; a busy batch
   item becomes that item's own outcome, re-asked by the core.
1. **Observation delivery.** `watch` (latest) vs queue. **Recommend `watch`** (§6).
1. **Borrowing.** Reserve + ceiling vs no-borrow lanes. **Recommend reserve + ceiling**: a wallet
   burst uses idle bulk capacity, and the poll's reserve still holds.
1. **Abandon.** Dropping an ask's future abandons it (in-flight sends cancelled). **Recommend
   yes**: NFS drops wants off the best chain by dropping futures, no cancel API.
