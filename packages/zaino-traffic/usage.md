# `zaino-traffic` — usage

One scheduler for every request Zaino sends to its trusted validators and to peers
(`docs/design/traffic-balancer.md`): one member table, per member a permit for its poll and three
lanes (control, interactive, bulk) each with a reserved permit, one hedge / failover / blame
policy, and the poll loop of every trusted validator.

## Wiring

```rust,ignore
use zaino_traffic::{Limits, TrafficBalancer, Trusted};

let trusted = validators
    .iter()
    .map(|v| Trusted {
        source: Arc::new(ZebraRpcAdapter::at(&v.address, cookie, user, password, timeouts, link)?),
        priority: v.priority,                                  // 0 before 1 before …
        limits: Limits::new(v.max_connections, None)?,         // None below 5
    })
    .collect();
let (balancer, driver) = TrafficBalancer::new(trusted, Some(peer_transport));
tasks.spawn(driver.run(cancel));     // without it, asks pend and nobody is polled
```

- `trusted[i]` is `ValidatorId(i)`: configured order, at most `ValidatorId::MAX` (64).
- `S: ChainDataSource` (`zaino-source`): `ZebraRpcAdapter` in production, `zaino_source::testing::MockValidator` in tests.
- `peers`: `Option<Arc<dyn PeerTransport>>`. `joined_left()` adds and removes peers as members;
  `None` = trusted only.
- `Limits::new(max_connections)`; `Limits::MIN_CONNECTIONS` = 5: one for the poll, one reserved
  per lane + one shared.
- Load on a validator = `max_connections` here + `LinkLimits::max_bytes_per_sec` in the
  transport. There is no request-rate limit.
- `TrafficBalancer` is `Clone` (one `Arc`); hand the same one to every consumer.

## Asking

Every answer is `Answered { value, from, ticket }`: `from` names the member that sent it.

| Method | Class (lane) | Reaches | Ends |
| --- | --- | --- | --- |
| `block(hash, Urgency::Tip / Bulk)` | `TipBlock` (interactive) / `BulkBlock` (bulk) | trusted by tier, then peers | never unanswered: every member tried → next round after 1 s |
| `headers(HeaderAsk::Pinned { member, heights })` | `Headers` (control) | that trusted member only | unanswered if it fails or is out |
| `headers(HeaderAsk::Peers { locator, stop })` | `Headers` (control) | any peer | unanswered when every peer is tried |
| `bytes(listed, prefer)` | `Bytes` (interactive) | best tier, `prefer` first within it | one batch from one member |
| `transaction(txid)` | `Lookup` (interactive) | trusted only, absent → next | unanswered once every one is tried |
| `submit(member, raw)` | `Submit` (control) | that trusted member only | one attempt |

- Within the best tier with room: the route's preferred member, else the least in flight (ties:
  configured order). A freed permit goes to the control lane first, then interactive, then bulk.

- `Unanswered { last }`: the first transport failure, else the last domain answer (absent,
  rejected); `None` = no eligible member to ask at all (benched, down, none of that kind).
- Dropping the future abandons the ask: its sends are dropped and their permits return. There is
  no cancel API.
- Hedges (`TipBlock` 2 s, `Lookup` 1 s, `BulkBlock` 15 s after the latest send) go to another
  member; the first value wins and the rest are dropped.
- A failure moves on to the next member; a round asks each member once. Do not resend in the
  transport.

## Blame: `report`

The balancer cannot judge a value; the caller does (`check_block`, header rule H8, txid
recomputed from bytes). On failure:

```rust,ignore
let checked = loop {
    let answered = balancer.block(hash, Urgency::Tip).await;
    match check_block(answered.value, height, &record) {
        Ok(checked) => break checked,
        // benches `answered.from` alone; the re-ask never reaches it while benched
        Err(why) => balancer.report(answered.ticket, &why),
    }
};
```

- Bench = 60 s × 2^(times − 1), at most 1 h; even the last trusted member (finality pausing is
  the right answer to a lying validator). Counted in `zaino_traffic_misanswers_total{member,class}`.
- Absent is never blame: a lagging validator lacks a fresh block. A header from the future (H7)
  or an orphan run is not a misanswer either.

## Polling and observations

- Each trusted member is polled every 1 s (every 15 s while its push stream is up), never closer
  than 200 ms apart, on the 0.5 → 30 s ladder while failing; a `Down` member (10 consecutive
  failures) is probed at 30 s and nothing else is sent to it. The poll has a permit of its own,
  outside every lane: no ask ever delays it. Metadata (`getpeerinfo`, `getinfo`,
  `getdeprecationinfo`) rides one poll a minute; a failed poll leaves it due.
- `poll_best(f)`: every poll asks `getblockhash` at `f()` (the caller's best height, `None` =
  not asked), read as the poll starts; it never wakes a poll (an answer at a moved best is one
  stale fact, re-asked next poll). `observe(member)` is a `watch` of the latest
  `Observation { polled, asked, at, streaming, health }` (`asked` = that `f()`; latest only: a
  slow consumer never stalls a poll; `health` = the member's right after that poll, so a
  consumer folds `Degraded` vs `Down` without the table).
  A consumer subscribing after the driver started marks the watch changed to read the poll
  already there.
- `pushed(member, Push::Changed | Push::Link(up))`: wire `IndexerWatch::run`'s callbacks here; an
  event polls within 200 ms.
- `members()`: `watch` of the `MemberTable` (health, consecutive failures, bench, last reply's
  latency, in flight; trusted first, configured order) for `/statusz` and metrics, refreshed by
  the driver. `entries()`: trusted members a submission may enter by (live, not benched).

## Health

`Pending` (never polled) → `Live` / `CatchingUp` (polled; catching up = no mempool, so never
asked lookups or bytes) → `Degraded` (consecutive failures, any request) → `Down` (10).
`Benched` is orthogonal: a fast liar is `Live` and benched. `Health::label()` = the metric label
(`pending`, `live`, `catching_up`, `degraded`, `down`).

`Unanswered` displays its `last` (or "no member to ask"), for logs and wallet-facing statuses.
