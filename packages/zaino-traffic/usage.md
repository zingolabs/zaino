# `zaino-traffic` — usage

One scheduler for every request Zaino sends to its trusted validators and to peers
([architecture §9](../../docs/architecture.md#9-talking-to-validators-and-peers)): one member
table, one hedge / failover / blame policy, and the poll loop of every trusted validator. Per
member, one permit is the poll's alone (nothing delays tip following); then three lanes, control
(submit, headers), interactive (tip blocks, lookups, mempool bytes) and bulk (bulk blocks), each
with one reserved permit; the rest is shared.

Built as a pure core (`TrafficCore::step(input, now) -> Vec<Output>`, time as input) plus one
driver. No `tower::Service`: every answer must name its sender (blame) and every hedge needs
"anyone but him".

## Wiring

```rust,ignore
use zaino_traffic::{Limits, Peers, TrafficBalancer, TrustedValidator};

let trusted = validators
    .iter()
    .map(|v| TrustedValidator {
        address: v.address.clone(),                            // its name in logs and status
        source: Arc::new(ZcashLegacyRpcClient::new(&v.endpoint)?),
        priority: v.priority,                                  // 0 before 1 before …
        limits: Limits::new(v.max_connections)?,               // None below 5
    })
    .collect();
let peers = Peers { transport: Arc::new(peer_transport), in_flight: peer_target };
let (balancer, driver) = TrafficBalancer::new(trusted, Some(peers));
tasks.spawn(driver.run(cancel));     // without it, asks pend and nobody is polled
```

- `trusted[i]` is `TrustedValidatorId(i)`: configured order, 1 ..= `TrustedValidatorId::MAX`
  (64); `TrafficBalancer::new` panics outside that, so a config is refused before (zainod's
  `validate`). `addresses()[i]` = its address, the chain view's name for it.
- `S: ChainDataSource` (`zaino-source`): `ZcashLegacyRpcClient` or `ZebraIndexerGrpcClient` in
  production (one type per balancer: zainod's `validator_client`); in tests the same
  `ZcashLegacyRpcClient`, served by a `zaino_source::testing::MockValidator`
  (`validator.client()`).
- `peers`: `Option<Peers>`, the p2p network's whole peer set as one member (`MemberId::Peers`,
  below every trusted tier); `None` = trusted only. Its transport (`PeerTransport`: `headers`,
  `block`) picks the peer itself and must check a block body against its header, blaming the
  peer that sent a mismatch. `in_flight` = requests it carries at once, at least
  `Peers::MIN_IN_FLIGHT` (3: one reserved per lane; it has no poll).
- The peer set answers headers and blocks only and never goes `Down`. `report` on its ticket logs
  and counts but never benches it (one peer's lie must not starve the honest ones).
- `Limits::new(max_connections)`; `Limits::MIN_CONNECTIONS` = 5: one for the poll, one reserved
  per lane + one shared. `Limits::default()` = 32. `max_connections()` reads it back (the
  transport's pool takes the same number). `Limits` (de)serializes as that bare number,
  refused at parse below 5: zainod's `[[trusted_validators]] max_connections`.
- Load on a validator = `max_connections` here ([architecture](../../docs/architecture.md#9-talking-to-validators-and-peers)).
- `TrafficBalancer` is `Clone` (one `Arc`); hand the same one to every consumer.

## Asking

Every answer is `Answered { value, ticket }`: `ticket` names the send that answered (`report` takes
it).

| Method | Class (lane) | Reaches | Ends |
| --- | --- | --- | --- |
| `block(hash, Urgency::Tip / Bulk)` | `TipBlock` (interactive) / `BulkBlock` (bulk) | trusted by tier, then the peer set | never unanswered: every member tried → next round after 1 s, a warning each minute it stays unserved |
| `blocks_at(heights, urgency)` | same classes | trusted only (peers answer by hash): `getblock <h> 0` batches (sized by `ResponseSizeCap`), or one `GetBlockRange` per run over gRPC | never unanswered, as `block` |
| `headers(HeaderAsk::Pinned { validator, heights })` | `Headers` (control) | that trusted validator only | unanswered if it fails or is out |
| `headers(HeaderAsk::Peers { locator })` | `Headers` (control) | the peer set | unanswered if it fails |
| `bytes(listed, prefer)` | `Bytes` (interactive) | trusted, best tier, `prefer` first within it | one batch from one member |
| `lookup(ask)` | `Lookup` (interactive) | trusted only, a domain answer or `Unsupported` → next | unanswered once every one is tried |
| `peer_block_bytes(hash)` | `TipBlock` (interactive) | the peer set only (`PeerTransport::block_bytes`) | `None` with no peer set, else never unanswered, as `block` |
| `submit(member, ask)` | `Submit` (control) | that trusted member only | one attempt |

`lookup` and `submit` name a traffic class, not an RPC: `ask` is the `ChainDataSource` call,
so a new forwarded method needs no balancer change:

```rust,ignore
balancer.lookup(move |source| async move { source.get_transaction(txid).await }).await
balancer.submit(member, move |source| {
    let raw = raw.clone();                     // one attempt, but `ask` is `Fn`
    async move { source.check_admission(raw).await }
}).await
```

- Within the best tier with room: the route's preferred member, else the least in flight (ties:
  configured order). A freed permit goes to the control lane first, then interactive, then bulk.

- `Unanswered { last }`: the first transport failure, else the last domain answer (absent,
  rejected), else `Unsupported` (every member asked lacks the method: never a failure, health
  untouched); `None` = no eligible member to ask at all (benched, down, none of that kind).
- Dropping the future abandons the ask: its sends are dropped and their permits return. There is
  no cancel API.
- Hedges (`TipBlock` 2 s, `Lookup` 1 s, `BulkBlock` 15 s after the latest send) go to another
  member; the first value wins and the rest are dropped.
- A failure moves on to the next member; a round asks each member once. Do not resend in the
  transport.

## Blame: `report`

The balancer cannot judge a value; the caller does (`zaino_sync::fetch`'s body check, the header rules,
txid recomputed from bytes). On failure:

```rust,ignore
let checked = loop {
    let answered = balancer.block(hash, Urgency::Tip).await;
    match check_block(answered.value, height, &record) {
        Ok(checked) => break checked,
        // benches the ticket's member alone; the re-ask never reaches it while benched
        Err(why) => balancer.report(answered.ticket, &why),
    }
};
```

- Bench = 60 s × 2^(times − 1), at most 1 h; even the last trusted member (finality pausing is
  the right answer to a lying validator). Counted in
  `zaino_traffic_misanswers_total{endpoint,class}` (`endpoint` = its address, `peers` for the
  peer set).
- Absent is never blame: a lagging validator lacks a fresh block. A header from the future
  or an orphan run is not a misanswer either.

## Polling and observations

- Each trusted member is polled every 1 s (every 15 s while both its push streams are up), never
  closer than 200 ms apart, on the 0.5 → 30 s ladder while failing; a `Down` member (10 consecutive
  failures) is probed at 30 s and nothing else is sent to it. The poll has a permit of its own,
  outside every lane: no ask ever delays it (its calls: `ChainDataSource::get_poll_reading`).
  Metadata rides one poll a minute; a failed poll leaves it due.
- `poll_best(f)`: every poll asks `getblockhash` at `f()` (the caller's best height, `None` =
  not asked), read as the poll starts; it never wakes a poll (an answer at a moved best is one
  stale fact, re-asked next poll). `observe(member)` is a `watch` of the latest
  `Observation { polled, asked, streaming, health }` (`asked` = that `f()`; latest only: a
  slow consumer never stalls a poll; `health` = the member's right after that poll, so a
  consumer folds `Degraded` vs `Down` from it).
  A consumer subscribing after the driver started marks the watch changed to read the poll
  already there.
- `pushed(member, Push::Changed | Push::Link(up))`: wire `ZebraIndexerWatch::run`'s callbacks
  here; an event polls within 200 ms.
- `entries()`: trusted members a submission may enter by (live, not benched).

## Health

`Pending` (never polled) → `Live` / `CatchingUp` (polled; catching up = no mempool, so never
asked lookups or bytes) → `Degraded` (consecutive failures, any request) → `Down` (10).
`Benched` is orthogonal: a fast liar is `Live` and benched. `Health::label()` = the metric label
(`pending`, `live`, `catching_up`, `degraded`, `down`).

Each change is logged once, by its address (`was` = the label left): INFO `Validator live`, WARN
`Validator mempool inactive (catching up)`, `Validator failing`, `Validator down, probed until it
answers`. A failed poll also WARNs its cause, except while `Down`.

`Unanswered` displays its `last` (or "no member to ask"), for logs and wallet-facing statuses.

## Invariants

`TrafficCore::check()` (T1, T2, T4, T5, T10) is test-only: the model calls it after every step;
its oracle checks T3 and T6 on every send.

| ID | Invariant |
| --- | --- |
| T1 | per member: Σ in flight ≤ `max_connections`; a lane's reserve never taken by another lane |
| T2 | a class reaches only the member kinds it allows (trusted-only never reaches the peer set) |
| T3 | no new send to a benched or `Down` member (except its poll probe) |
| T4 | every ask ends once: answered, unanswered or abandoned; a hedge loser is never delivered |
| T5 | a round never asks one member twice; a re-ask after `report` excludes the reported member |
| T6 | a send (first or hedge) goes to the best tier with room, then the preferred, then the least loaded |
| T7 | no ready ask waits while an eligible member has room for its lane; a shared permit never goes to a lower lane while a higher lane's ask waits for that member |
| T8 | every misanswer is charged to the member that sent it, and only to it |
| T9 | an ask an honest member may serve is answered; with a faster honest member, within hedge floor + one round |
| T10 | a trusted member is polled ≤ once per 200 ms and ≥ once per interval (ladder while failing), never delayed by an ask |

## Tests

- Feature `testing`: `testing::MockPeerSet::new(&chain, tip)` gives the peer set as a
  `PeerTransport` over one `MockChain` path, as `ZebraPeers` answers: headers after the first
  locator hash held (`FindHeaders`, ≤ 160), blocks by hash, an unheld block = `Connection`.

  ```rust,ignore
  use zaino_traffic::testing::MockPeerSet;

  let peer_set = Arc::new(MockPeerSet::new(&chain, at_60));   // genesis ..= 60 held
  peer_set.hold(h(40));                             // the network not past 40 yet
  let peers = Peers { transport: Arc::clone(&peer_set) as _, in_flight: 4 };
  ```

- `core/model_tests.rs` (proptest): members `Honest`, `Slow`, `Lying`, `Lagging`, `Flapping`, `Down`,
  `Silent`, the peer set present or not; random asks, abandons, reports, clock advances against an
  oracle with its own lanes and permits. A bound matrix pins each class's end time to its hedge
  floor + one round.
- `core/fire_drills.rs`: one planted bug per `check()` assertion and precondition.
- `tests.rs` (paused clock): a lookup storm never delays a poll, a merkle liar is benched, a hedge
  beats a 20 s stall, a push stream wakes the poll within 200 ms.

After any change to this crate: the [heavy run](../../docs/testing.md#heavy-run).
